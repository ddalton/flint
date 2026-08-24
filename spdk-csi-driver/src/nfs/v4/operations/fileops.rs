// NFSv4 Basic File Operations
//
// This module implements core file operations for NFSv4:
//
// Note: Some FATTR4_* constants and helper functions are defined for RFC 5661
// protocol completeness but not yet used in the current implementation.
#![allow(dead_code)]
// - File handle operations: PUTROOTFH, PUTFH, GETFH, SAVEFH, RESTOREFH
// - Navigation: LOOKUP, LOOKUPP
// - Attributes: GETATTR, SETATTR
// - Directory: READDIR
// - Access: ACCESS
//
// These operations work with the COMPOUND context's current/saved filehandles.

use crate::nfs::v4::protocol::*;
use crate::nfs::v4::compound::{CompoundContext, ChangeInfo, DirEntry as CompoundDirEntry};
use crate::nfs::v4::filehandle::FileHandleManager;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH, Duration};
use tracing::{debug, info, warn};
use bytes::{BufMut, BytesMut};

/// Build a RENAME error reply with empty cinfo. Centralised so the validation
/// chain in `handle_rename` can early-return without repeating the struct.
fn rename_err(status: Nfs4Status) -> RenameRes {
    RenameRes { status, source_cinfo: None, target_cinfo: None }
}

/// Translate UID to NFSv4 owner string
///
/// Using "root" or "root@domain" causes ID mapping issues when domain
/// configuration doesn't match - client maps to nobody (UID 65534).
///
/// Numeric strings (e.g., "0", "1000") are universally recognized by
/// Linux NFS client's idmapper and avoid domain configuration issues.
fn uid_to_username(uid: u32) -> String {
    // Use numeric string 
    // This avoids ID mapping failures when domain config is missing/mismatched
    uid.to_string()
}

/// Translate GID to NFSv4 group string
///
/// Using "root" or "root@domain" causes ID mapping issues when domain
/// configuration doesn't match - client maps to nogroup (GID 65534).
///
/// Numeric strings (e.g., "0", "1000") are universally recognized by
/// Linux NFS client's idmapper and avoid domain configuration issues.
fn gid_to_groupname(gid: u32) -> String {
    // Use numeric string 
    // This avoids ID mapping failures when domain config is missing/mismatched
    gid.to_string()
}

/// Point-in-time snapshot of file attributes
///
/// Per RFC 8434 §13, all attributes returned in a single response MUST represent
/// a consistent point-in-time snapshot. This struct captures all file attributes
/// from a SINGLE VFS call, ensuring consistency.
///
/// Key principle: Fetch ONCE, encode MANY times
#[derive(Debug, Clone)]
pub struct AttributeSnapshot {
    // Basic type
    pub ftype: u32,        // NF4REG, NF4DIR, NF4LNK, etc.
    
    // Size and space
    pub size: u64,
    pub space_used: u64,
    
    // Identity
    pub fileid: u64,
    pub fsid_major: u64,
    pub fsid_minor: u64,
    
    // Times (all from same stat() call)
    pub atime: SystemTime,
    pub mtime: SystemTime,
    pub ctime: SystemTime,
    pub change: u64,       // Change attribute (ctime-based)
    
    // Permissions and ownership
    pub mode: u32,
    pub numlinks: u32,
    pub owner: u32,
    pub group: u32,
    
    // Source (for debugging)
    pub path: PathBuf,
}

impl AttributeSnapshot {
    /// Create a snapshot from filesystem metadata
    ///
    /// This performs a SINGLE stat() call and captures all attributes atomically.
    /// This is the ONLY place where VFS I/O should happen for attribute queries.
    /// 
    /// IMPORTANT: Uses symlink_metadata() to NOT follow symlinks (lstat vs stat)
    pub async fn from_path(path: &Path) -> std::io::Result<Self> {
        // Use symlink_metadata() instead of metadata() to get symlink's own attributes
        // This is equivalent to lstat() vs stat() - returns the symlink itself, not target
        let metadata = tokio::fs::symlink_metadata(path).await?;
        Self::from_metadata(metadata, path)
    }

    /// Create a snapshot from already-fetched metadata. Used by
    /// `from_path` (lstat) and by the open-file fallback (fstat on a
    /// cached fd — the file may be renamed-over/unlinked, alive only
    /// through server-held opens; F17b).
    pub fn from_metadata(metadata: std::fs::Metadata, path: &Path) -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            
            // Determine file type
            let ftype = if metadata.is_dir() {
                2  // NF4DIR
            } else if metadata.is_symlink() {
                5  // NF4LNK
            } else {
                1  // NF4REG
            };
            
            // Get times (all from same metadata)
            let atime = metadata.accessed().unwrap_or(SystemTime::UNIX_EPOCH);
            let mtime = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            let ctime_secs = metadata.ctime() as u64;
            let ctime = SystemTime::UNIX_EPOCH + Duration::from_secs(ctime_secs);

            // Step 10 (C2): an EVICTED file's on-disk length is 0 (the
            // stub); GETATTR must serve the LOGICAL size from its
            // marker or every `ls -l` reads as truncation. space_used
            // stays physical — "0 blocks, full size" is also exactly
            // how the marker reads in forensics.
            let size = if ftype == 1 {
                crate::tier::evict::logical_size(metadata.dev(), metadata.ino())
                    .unwrap_or_else(|| metadata.len())
            } else {
                metadata.len()
            };
            Ok(Self {
                ftype,
                size,
                space_used: metadata.blocks() * 512, // blocks are typically 512 bytes
                fileid: metadata.ino(),
                fsid_major: metadata.dev(),
                fsid_minor: 0,
                atime,
                mtime,
                ctime,
                // The change attr is the client's cache-ordering key.
                // Whole seconds (F13) — and even raw ctime ns (F14: ext4's
                // clock ticks at jiffy granularity, so create+write in one
                // tick TIE) — let an out-of-order GETATTR reply carrying a
                // stale/shorter size win the client's cache race: pgbench
                // "unexpected data beyond EOF", postmaster.pid read back
                // as 0. Report the server's per-file mutation counter
                // floored by ctime ns (see change_counter.rs).
                change: crate::nfs::v4::change_counter::current(
                    metadata.dev(),
                    metadata.ino(),
                    ctime_secs
                        .wrapping_mul(1_000_000_000)
                        .wrapping_add(metadata.ctime_nsec() as u64),
                ),
                mode: metadata.mode(),
                numlinks: metadata.nlink() as u32,
                owner: metadata.uid(),
                group: metadata.gid(),
                path: path.to_path_buf(),
            })
        }
        
        #[cfg(not(unix))]
        {
            // Non-Unix fallback (Windows, etc.)
            let ftype = if metadata.is_dir() { 2 } else { 1 };
            let mtime = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            let atime = mtime; // Windows doesn't always have atime
            
            Ok(Self {
                ftype,
                size: metadata.len(),
                space_used: metadata.len(), // Approximate
                fileid: 0, // Not available on Windows
                fsid_major: 0,
                fsid_minor: 0,
                atime,
                mtime,
                ctime: mtime,
                change: mtime.duration_since(UNIX_EPOCH).unwrap().as_secs(),
                mode: if metadata.is_dir() { 0o755 } else { 0o644 },
                numlinks: 1,
                owner: 0,
                group: 0,
                path: path.to_path_buf(),
            })
        }
    }
    
    /// Create a synthetic snapshot for pseudo-root
    ///
    /// Pseudo-root doesn't have a real filesystem path, so we create
    /// synthetic attributes per RFC 7530 Section 7.
    pub fn pseudo_root(num_exports: usize) -> Self {
        let now = SystemTime::now();
        let change = now.duration_since(UNIX_EPOCH).unwrap().as_secs();
        
        Self {
            ftype: 2, // NF4DIR
            size: 4096,
            space_used: 4096,
            fileid: 1, // Pseudo-root always has fileid 1
            fsid_major: 0, // FSID (0, 0) indicates pseudo-fs
            fsid_minor: 0,
            atime: now,
            mtime: now,
            ctime: now,
            change,
            mode: 0o755,
            numlinks: 2 + num_exports as u32, // . .. and exports
            owner: 0, // root
            group: 0, // root
            path: PathBuf::from("/"),
        }
    }
}

/// PUTROOTFH operation (opcode 24)
///
/// Sets current filehandle to the root of the export.
pub struct PutRootFhOp;

pub struct PutRootFhRes {
    pub status: Nfs4Status,
}

/// PUTFH operation (opcode 22)
///
/// Sets current filehandle to the specified handle.
pub struct PutFhOp {
    pub filehandle: Nfs4FileHandle,
}

pub struct PutFhRes {
    pub status: Nfs4Status,
}

/// GETFH operation (opcode 10)
///
/// Returns the current filehandle.
pub struct GetFhOp;

pub struct GetFhRes {
    pub status: Nfs4Status,
    pub filehandle: Option<Nfs4FileHandle>,
}

/// SAVEFH operation (opcode 32)
///
/// Saves the current filehandle to saved filehandle.
pub struct SaveFhOp;

pub struct SaveFhRes {
    pub status: Nfs4Status,
}

/// RESTOREFH operation (opcode 30)
///
/// Restores saved filehandle to current filehandle.
pub struct RestoreFhOp;

pub struct RestoreFhRes {
    pub status: Nfs4Status,
}

/// LOOKUP operation (opcode 15)
///
/// Looks up a component in the current directory.
pub struct LookupOp {
    pub component: String,
}

pub struct LookupRes {
    pub status: Nfs4Status,
}

/// LOOKUPP operation (opcode 16)
///
/// Looks up parent directory.
pub struct LookupPOp;

pub struct LookupPRes {
    pub status: Nfs4Status,
}

/// GETATTR operation (opcode 9)
///
/// Gets attributes for current filehandle.
pub struct GetAttrOp {
    pub attr_request: Vec<u32>, // Bitmap of requested attributes
}

pub struct GetAttrRes {
    pub status: Nfs4Status,
    pub obj_attributes: Option<Fattr4>,
}

/// Fattr4 - NFSv4 file attributes
#[derive(Debug, Clone)]
pub struct Fattr4 {
    pub attrmask: Vec<u32>,
    pub attr_vals: Vec<u8>, // XDR-encoded attribute values
}

/// A settime4 value from SETATTR / OPEN createattrs (RFC 8881 §3.3.5).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SetTime {
    ServerTime,
    ClientTime { seconds: i64, nseconds: u32 },
}

impl SetTime {
    fn to_system_time(self) -> std::time::SystemTime {
        match self {
            SetTime::ServerTime => std::time::SystemTime::now(),
            SetTime::ClientTime { seconds, nseconds } => {
                let base = std::time::UNIX_EPOCH;
                if seconds >= 0 {
                    base + std::time::Duration::new(seconds as u64, nseconds)
                } else {
                    base - std::time::Duration::new(seconds.unsigned_abs(), 0)
                }
            }
        }
    }
}

/// The client-settable attributes we support, decoded from a fattr4
/// (RFC 8881 §5.6). OWNER / OWNER_GROUP carry numeric ids ("999" or
/// "999@domain" — the kernel client sends numeric strings with
/// idmapping off, the default under sec=sys) and are applied to the
/// backing object via chown; GETATTR already reports the backing
/// uid/gid, so ownership round-trips. TIME_CREATE is consumed to keep
/// the XDR cursor aligned but ignored.
#[derive(Debug, Default, Clone)]
pub struct SettableAttrs {
    pub size: Option<u64>,
    pub mode: Option<u32>,
    pub owner: Option<u32>,
    pub owner_group: Option<u32>,
    pub atime: Option<SetTime>,
    pub mtime: Option<SetTime>,
}

/// Parse a fattr4_owner / fattr4_owner_group value: a numeric id,
/// optionally with an "@domain" suffix. Non-numeric principals (no
/// idmapping here) → NFS4ERR_BADOWNER per RFC 8881 §5.9.
fn parse_owner4(bytes: &[u8]) -> Result<u32, Nfs4Status> {
    let s = std::str::from_utf8(bytes).map_err(|_| Nfs4Status::BadOwner)?;
    let num = s.split('@').next().unwrap_or("").trim_end_matches('\0');
    num.parse::<u32>().map_err(|_| Nfs4Status::BadOwner)
}

/// Decode the settable subset of a fattr4 (bitmap words + packed attr
/// values). Values are packed in ascending attr-number order.
///
/// Errors per RFC 8881 §18.30.4: a recognized-but-read-only attr →
/// `INVAL`; a writable attr we don't support → `ATTRNOTSUPP`. Both are
/// hard errors *before* anything is applied — an unknown attr has an
/// unknown wire size, so nothing after it can be decoded anyway.
pub fn decode_settable_attrs(
    attrmask: &[u32],
    attr_vals: &[u8],
) -> Result<SettableAttrs, Nfs4Status> {
    struct Cursor<'a> {
        buf: &'a [u8],
        pos: usize,
    }
    impl<'a> Cursor<'a> {
        fn take(&mut self, n: usize) -> Result<&'a [u8], Nfs4Status> {
            let end = self.pos.checked_add(n).ok_or(Nfs4Status::BadXdr)?;
            if end > self.buf.len() {
                return Err(Nfs4Status::BadXdr);
            }
            let s = &self.buf[self.pos..end];
            self.pos = end;
            Ok(s)
        }
        fn u32(&mut self) -> Result<u32, Nfs4Status> {
            Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
        }
        fn u64(&mut self) -> Result<u64, Nfs4Status> {
            Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))
        }
        fn i64(&mut self) -> Result<i64, Nfs4Status> {
            Ok(i64::from_be_bytes(self.take(8)?.try_into().unwrap()))
        }
        fn opaque(&mut self) -> Result<&'a [u8], Nfs4Status> {
            let len = self.u32()? as usize;
            let padded = len.checked_add(3).ok_or(Nfs4Status::BadXdr)? & !3;
            let s = self.take(padded)?;
            Ok(&s[..len])
        }
        fn settime(&mut self) -> Result<SetTime, Nfs4Status> {
            const SET_TO_CLIENT_TIME4: u32 = 1;
            if self.u32()? == SET_TO_CLIENT_TIME4 {
                Ok(SetTime::ClientTime { seconds: self.i64()?, nseconds: self.u32()? })
            } else {
                Ok(SetTime::ServerTime)
            }
        }
    }

    let mut cur = Cursor { buf: attr_vals, pos: 0 };
    let mut out = SettableAttrs::default();

    for (word_idx, word) in attrmask.iter().enumerate() {
        for bit in 0..32 {
            if word & (1 << bit) == 0 {
                continue;
            }
            let attr = word_idx as u32 * 32 + bit;
            match attr {
                FATTR4_SIZE => out.size = Some(cur.u64()?),
                FATTR4_MODE => out.mode = Some(cur.u32()? & 0o7777),
                FATTR4_OWNER => out.owner = Some(parse_owner4(cur.opaque()?)?),
                FATTR4_OWNER_GROUP => out.owner_group = Some(parse_owner4(cur.opaque()?)?),
                FATTR4_TIME_ACCESS_SET => out.atime = Some(cur.settime()?),
                FATTR4_TIME_MODIFY_SET => out.mtime = Some(cur.settime()?),
                FATTR4_TIME_CREATE => {
                    // nfstime4 — settable on filesystems with birth time;
                    // consume and ignore.
                    cur.i64()?;
                    cur.u32()?;
                }
                // Writable per the RFC but unsupported here.
                FATTR4_ACL | FATTR4_HIDDEN | FATTR4_MIMETYPE | FATTR4_SYSTEM
                | FATTR4_TIME_BACKUP => return Err(Nfs4Status::AttrNotsupp),
                // Everything else a client could name is read-only.
                _ => return Err(Nfs4Status::Inval),
            }
        }
    }
    Ok(out)
}

/// Apply decoded settable attrs to `path`. Returns the attr numbers
/// actually applied (for the SETATTR4res / OPEN4res attrset bitmap); on
/// failure returns what had been applied before the error, plus the
/// error, per RFC 8881 §18.30.4.
///
/// MODE is applied before SIZE on purpose: truncation needs a writable
/// open, and a compound that sets both may be un-hiding a 0o000 file.
pub fn apply_settable_attrs(
    path: &Path,
    want: &SettableAttrs,
) -> (Vec<u32>, Option<Nfs4Status>) {
    let out = apply_settable_attrs_inner(path, want);
    // F14: any applied attr is a mutation the change attribute must
    // reflect ahead of colliding ctime ticks (partial application on
    // error included — something changed either way).
    if !out.0.is_empty() {
        crate::nfs::v4::change_counter::bump_path(path);
    }
    out
}

fn apply_settable_attrs_inner(
    path: &Path,
    want: &SettableAttrs,
) -> (Vec<u32>, Option<Nfs4Status>) {
    let mut applied: Vec<u32> = Vec::new();

    let lmeta = match path.symlink_metadata() {
        Ok(m) => m,
        Err(_) => return (applied, Some(Nfs4Status::NoEnt)),
    };
    let is_symlink = lmeta.file_type().is_symlink();

    // Owner FIRST: chown clears setuid/setgid bits, so a compound that
    // sets both owner and mode must not have its mode stomped.
    if want.owner.is_some() || want.owner_group.is_some() {
        #[cfg(unix)]
        {
            let res = if is_symlink {
                std::os::unix::fs::lchown(path, want.owner, want.owner_group)
            } else {
                std::os::unix::fs::chown(path, want.owner, want.owner_group)
            };
            match res {
                Ok(()) => {
                    want.owner.map(|_| applied.push(FATTR4_OWNER));
                    want.owner_group.map(|_| applied.push(FATTR4_OWNER_GROUP));
                }
                Err(e) => {
                    warn!(
                        "SETATTR: chown {:?}:{:?} on {:?} failed: {}",
                        want.owner, want.owner_group, path, e
                    );
                    return (applied, Some(io_error_to_nfs4(&e)));
                }
            }
        }
    }

    if let Some(mode) = want.mode {
        if is_symlink {
            // Symlink modes are fixed on Unix; treat as a successful no-op
            // (pynfs clean_dir SETATTRs symlinks before REMOVE).
            applied.push(FATTR4_MODE);
        } else {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                match std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)) {
                    Ok(()) => applied.push(FATTR4_MODE),
                    Err(e) => {
                        warn!("SETATTR: chmod {:o} on {:?} failed: {}", mode, path, e);
                        return (applied, Some(io_error_to_nfs4(&e)));
                    }
                }
            }
        }
    }

    if let Some(size) = want.size {
        if is_symlink {
            return (applied, Some(Nfs4Status::Inval));
        }
        if lmeta.is_dir() {
            return (applied, Some(Nfs4Status::IsDir));
        }
        // A4 write gate, spanning the truncate AND its capture notes
        // (both size lanes — SETATTR and OPEN-createattrs — pass this
        // one chokepoint). An excluded file refuses with DELAY.
        let _gate = match crate::tier::gate::enter_path(path) {
            Ok(t) => t,
            Err(crate::tier::gate::Excluded) => {
                return (applied, Some(Nfs4Status::Delay));
            }
        };
        // Step 10: size changes to an EVICTED file park like writes —
        // a truncate against the 0-byte stub is not a truncate of the
        // data. Step 11: hydrate-first, with WRITE priority.
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if crate::tier::evict::is_evicted(lmeta.dev(), lmeta.ino()) {
                // Blocked ⇒ NOSPC, not DELAY: parking a truncate on a
                // restore that can never be admitted wedges the caller.
                if let crate::tier::hydrate::Verdict::Blocked(_) =
                    crate::tier::hydrate::request(
                        lmeta.dev(),
                        lmeta.ino(),
                        path,
                        crate::tier::hydrate::Trigger::Write,
                    )
                {
                    return (applied, Some(Nfs4Status::NoSpc));
                }
                crate::tier::meter::bump(crate::tier::meter::Counter::EvictedOpDelays);
                return (applied, Some(Nfs4Status::Delay));
            }
        }
        // The `is_symlink` guard above already answers INVAL for a link,
        // but it was read from a `symlink_metadata` taken at the top of
        // this function — a whole sequence of chown/chmod ago. Going
        // through open_beneath makes the refusal atomic with the
        // truncate rather than trusting that nothing swapped the name in
        // between.
        match crate::nfs::v4::open_beneath::open(
            std::fs::OpenOptions::new().write(true),
            path,
        )
        .and_then(|f| f.set_len(size))
        {
            Ok(()) => {
                // A2 dirty capture, at the ONE chokepoint both size
                // lanes share (SETATTR and OPEN-createattrs both land
                // here). Shrink is a first-class Truncate event (clips
                // the log, pins the copy watermark); grow is the
                // kernel's zero-fill of the gap, noted as such — the
                // capture module cannot know the pre-op size, so the
                // split lives here where lmeta does.
                let old = lmeta.len();
                if size < old {
                    crate::tier::capture::note_path(
                        path,
                        crate::tier::capture::Mutation::Truncate { new_size: size },
                    );
                } else if size > old {
                    crate::tier::capture::note_path(
                        path,
                        crate::tier::capture::Mutation::Zero {
                            offset: old,
                            len: size - old,
                        },
                    );
                }
                applied.push(FATTR4_SIZE)
            }
            Err(e) => {
                warn!("SETATTR: truncate to {} on {:?} failed: {}", size, path, e);
                return (applied, Some(io_error_to_nfs4(&e)));
            }
        }
    }

    if want.atime.is_some() || want.mtime.is_some() {
        if is_symlink {
            // futimens through File::open would follow the link; skip and
            // report set — the times on the link itself are cosmetic.
            want.atime.map(|_| applied.push(FATTR4_TIME_ACCESS_SET));
            want.mtime.map(|_| applied.push(FATTR4_TIME_MODIFY_SET));
        } else {
            let mut times = std::fs::FileTimes::new();
            if let Some(t) = want.atime {
                times = times.set_accessed(t.to_system_time());
            }
            if let Some(t) = want.mtime {
                times = times.set_modified(t.to_system_time());
            }
            match crate::nfs::v4::open_beneath::open_read(path)
                .and_then(|f| f.set_times(times))
            {
                Ok(()) => {
                    want.atime.map(|_| applied.push(FATTR4_TIME_ACCESS_SET));
                    want.mtime.map(|_| applied.push(FATTR4_TIME_MODIFY_SET));
                }
                Err(e) => {
                    warn!("SETATTR: set times on {:?} failed: {}", path, e);
                    return (applied, Some(io_error_to_nfs4(&e)));
                }
            }
        }
    }

    (applied, None)
}

/// `apply_settable_attrs` on the blocking pool: its chmod/truncate/futimens
/// syscalls hit the export's backing device (a network block device in
/// production) and must not stall an async worker.
pub async fn apply_settable_attrs_offloaded(
    path: PathBuf,
    want: SettableAttrs,
) -> (Vec<u32>, Option<Nfs4Status>) {
    match tokio::task::spawn_blocking(move || apply_settable_attrs(&path, &want)).await {
        Ok(res) => res,
        Err(e) => {
            warn!("SETATTR: spawn_blocking error: {}", e);
            (Vec::new(), Some(Nfs4Status::ServerFault))
        }
    }
}

/// Build a bitmap4 (vec of words) from a list of attr numbers.
pub fn attr_numbers_to_bitmap(attrs: &[u32]) -> Vec<u32> {
    let mut words: Vec<u32> = Vec::new();
    for &a in attrs {
        let idx = (a / 32) as usize;
        if words.len() <= idx {
            words.resize(idx + 1, 0);
        }
        words[idx] |= 1 << (a % 32);
    }
    words
}

pub fn io_error_to_nfs4(e: &std::io::Error) -> Nfs4Status {
    // ELOOP here is not a symlink cycle — every by-path open in the data
    // path goes through `open_beneath`, which sets O_NOFOLLOW, so ELOOP
    // means "the object named is a symbolic link and the server declined
    // to follow it". RFC 8881 §18.16.3: that is NFS4ERR_SYMLINK, which
    // tells the client to READLINK and re-resolve on its own side —
    // where symlink resolution belongs. Reporting NFS4ERR_IO instead
    // would turn a normal, recoverable answer into a hard failure.
    if crate::nfs::v4::open_beneath::is_symlink_refusal(e) {
        return Nfs4Status::SymLink;
    }
    match e.kind() {
        std::io::ErrorKind::NotFound => Nfs4Status::NoEnt,
        std::io::ErrorKind::PermissionDenied => Nfs4Status::Access,
        _ => Nfs4Status::Io,
    }
}

/// RFC 8881 component-name validation, shared by every op that takes a
/// filename (LOOKUP, OPEN, CREATE, REMOVE, RENAME, LINK). A zero-length
/// name is `INVAL` (§18.10.3 and friends); "." / ".." / embedded '/' or
/// NUL are `BADNAME` — on a POSIX export those are not ordinary names,
/// and joining them into a path would allow escapes out of the export.
pub fn validate_component_name(name: &str) -> Option<Nfs4Status> {
    if name.is_empty() {
        return Some(Nfs4Status::Inval);
    }
    if name == "." || name == ".." || name.contains('/') || name.contains('\0') {
        return Some(Nfs4Status::BadName);
    }
    None
}

/// SETATTR operation (opcode 34)
///
/// Sets attributes for current filehandle.
pub struct SetAttrOp {
    pub stateid: StateId,
    pub obj_attributes: Fattr4,
}

pub struct SetAttrRes {
    pub status: Nfs4Status,
    pub attrsset: Vec<u32>, // Bitmap of attributes that were set
}

/// ACCESS operation (opcode 3)
///
/// Checks access permissions.
pub struct AccessOp {
    pub access: u32, // Bitmap of access to check
}

pub struct AccessRes {
    pub status: Nfs4Status,
    pub supported: u32, // Access bits supported
    pub access: u32,    // Access bits granted
}

/// Access bits (ACCESS4_*)
pub const ACCESS4_READ: u32 = 0x00000001;
pub const ACCESS4_LOOKUP: u32 = 0x00000002;
pub const ACCESS4_MODIFY: u32 = 0x00000004;
pub const ACCESS4_EXTEND: u32 = 0x00000008;
pub const ACCESS4_DELETE: u32 = 0x00000010;
pub const ACCESS4_EXECUTE: u32 = 0x00000020;

/// READDIR operation (opcode 26)
///
/// Reads directory entries.
pub struct ReadDirOp {
    pub cookie: u64,        // Position in directory
    pub cookieverf: u64,    // Cookie verifier
    pub dircount: u32,      // Max directory bytes
    pub maxcount: u32,      // Max response bytes
    pub attr_request: Vec<u32>, // Requested attributes for entries
}

pub struct ReadDirRes {
    pub status: Nfs4Status,
    pub cookieverf: u64,
    pub entries: Vec<CompoundDirEntry>,  // Use compound module's DirEntry (attrs: Bytes)
    pub eof: bool,
}

/// CREATE operation (opcode 6)
///
/// Creates a file or directory.
pub struct CreateOp {
    pub objtype: Nfs4FileType,
    pub objname: String,
    pub linkdata: Option<String>,  // For symlinks - target path
    pub createattrs: Fattr4,
}

pub struct CreateRes {
    pub status: Nfs4Status,
    pub change_info: Option<ChangeInfo>,
    pub attrset: Vec<u32>, // Which attributes were set
}

/// REMOVE operation (opcode 28)
///
/// Removes a file or directory.
pub struct RemoveOp {
    pub target: String, // Name of file/directory to remove
}

pub struct RemoveRes {
    pub status: Nfs4Status,
    pub change_info: Option<ChangeInfo>,
}

/// RENAME operation (opcode 29)
///
/// Renames a file or directory from saved FH to current FH.
/// Requires: saved_fh (source parent), current_fh (dest parent)
pub struct RenameOp {
    pub oldname: String, // Name in saved filehandle directory
    pub newname: String, // Name in current filehandle directory
}

pub struct RenameRes {
    pub status: Nfs4Status,
    pub source_cinfo: Option<ChangeInfo>,
    pub target_cinfo: Option<ChangeInfo>,
}

/// LINK operation (opcode 11)
///
/// Creates a hard link to current FH in saved FH directory.
/// Requires: current_fh (existing file), saved_fh (target directory)
pub struct LinkOp {
    pub newname: String, // Name for the new link
}

pub struct LinkRes {
    pub status: Nfs4Status,
    pub change_info: Option<ChangeInfo>,
}

/// READLINK operation (opcode 27)
///
/// Reads the target of a symbolic link.
pub struct ReadLinkOp;

pub struct ReadLinkRes {
    pub status: Nfs4Status,
    pub link: Option<String>, // Link target path
}

/// PUTPUBFH operation (opcode 23)
///
/// Sets current filehandle to the public filehandle.
/// Note: Public FH is rarely used, defaults to root FH.
pub struct PutPubFhOp;

pub struct PutPubFhRes {
    pub status: Nfs4Status,
}

// NFSv4 Attribute IDs (FATTR4_*) - Per RFC 5661 Table 3
const FATTR4_SUPPORTED_ATTRS: u32 = 0;
const FATTR4_TYPE: u32 = 1;
const FATTR4_FH_EXPIRE_TYPE: u32 = 2;
const FATTR4_CHANGE: u32 = 3;
const FATTR4_SIZE: u32 = 4;
const FATTR4_LINK_SUPPORT: u32 = 5;
const FATTR4_SYMLINK_SUPPORT: u32 = 6;
const FATTR4_NAMED_ATTR: u32 = 7;
const FATTR4_FSID: u32 = 8;

/// fsid major for scsi-class (pnfs-block) volumes — synthetic, chosen to
/// collide with no st_dev-derived value ("flint_bl" in ASCII). Each block
/// volume is its own filesystem: (SCSI_FSID_MAJOR, hash(volume)).
const SCSI_FSID_MAJOR: u64 = 0x666c_696e_745f_626c;

/// Stable per-volume fsid minor. DefaultHasher is deterministic across
/// processes and builds (the `stable_ns_identity` precedent) — the fsid
/// must not change across MDS restarts or the client remounts see a
/// "different" filesystem.
fn block_volume_fsid_minor(volume: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    ("flint-block-fsid", volume).hash(&mut h);
    h.finish()
}
const FATTR4_UNIQUE_HANDLES: u32 = 9;
const FATTR4_LEASE_TIME: u32 = 10;
const FATTR4_RDATTR_ERROR: u32 = 11;
const FATTR4_ACL: u32 = 12;         // FIXED: was 13 (swapped with ACLSUPPORT)
const FATTR4_ACLSUPPORT: u32 = 13;  // FIXED: was 12 — RFC 8881 §5.6: acl=12, aclsupport=13
const FATTR4_ARCHIVE: u32 = 14;
const FATTR4_CANSETTIME: u32 = 15;  // FIXED: was 35
const FATTR4_CASE_INSENSITIVE: u32 = 16;  // FIXED: was 39
const FATTR4_CASE_PRESERVING: u32 = 17;  // FIXED: was 40
const FATTR4_CHOWN_RESTRICTED: u32 = 18;
const FATTR4_FILEHANDLE: u32 = 19;
const FATTR4_FILEID: u32 = 20;
const FATTR4_FILES_AVAIL: u32 = 21;
const FATTR4_FILES_FREE: u32 = 22;
const FATTR4_FILES_TOTAL: u32 = 23;
const FATTR4_FS_LOCATIONS: u32 = 24;
const FATTR4_HIDDEN: u32 = 25;
const FATTR4_HOMOGENEOUS: u32 = 26;
const FATTR4_MAXFILESIZE: u32 = 27;  // FIXED: was 42
const FATTR4_MAXLINK: u32 = 28;  // FIXED: was 41
const FATTR4_MAXNAME: u32 = 29;  // FIXED: was 45
const FATTR4_MAXREAD: u32 = 30;  // FIXED: was 43
const FATTR4_MAXWRITE: u32 = 31;  // FIXED: was 44
const FATTR4_MIMETYPE: u32 = 32;
const FATTR4_MODE: u32 = 33;
const FATTR4_NO_TRUNC: u32 = 34;
const FATTR4_NUMLINKS: u32 = 35;  // FIXED: was 27
const FATTR4_OWNER: u32 = 36;
const FATTR4_OWNER_GROUP: u32 = 37;
const FATTR4_QUOTA_AVAIL_HARD: u32 = 38;
const FATTR4_QUOTA_AVAIL_SOFT: u32 = 39;
const FATTR4_QUOTA_USED: u32 = 40;
const FATTR4_RAWDEV: u32 = 41;  // ADDED: was missing
const FATTR4_SPACE_AVAIL: u32 = 42;  // FIXED: was 47
const FATTR4_SPACE_FREE: u32 = 43;  // FIXED: was 48
const FATTR4_SPACE_TOTAL: u32 = 44;  // FIXED: was 49
const FATTR4_SPACE_USED: u32 = 45;  // FIXED: was 50
const FATTR4_SYSTEM: u32 = 46;
const FATTR4_TIME_ACCESS: u32 = 47;  // FIXED: was 51
const FATTR4_TIME_ACCESS_SET: u32 = 48;
const FATTR4_TIME_BACKUP: u32 = 49;
const FATTR4_TIME_CREATE: u32 = 50;
const FATTR4_TIME_DELTA: u32 = 51;
const FATTR4_TIME_METADATA: u32 = 52;
const FATTR4_TIME_MODIFY: u32 = 53;
const FATTR4_TIME_MODIFY_SET: u32 = 54;
const FATTR4_MOUNTED_ON_FILEID: u32 = 55;
const FATTR4_SUPPATTR_EXCLCREAT: u32 = 75;
/// RFC 8881 §5.8.2.2 — how the server's CHANGE attribute behaves. We
/// advertise NFS4_CHANGE_TYPE_IS_MONOTONIC_INCR (0): change_counter
/// guarantees strictly increasing values per mutation, which lets the
/// kernel client ORDER attribute replies and discard stale ones (F14).
const FATTR4_CHANGE_ATTR_TYPE: u32 = 79; // word 2, bit 15

// pNFS attributes (RFC 8881 Section 5.12)
// NOTE: Using Linux kernel attribute numbers (not RFC 8881 numbers!)
// See: include/linux/nfs4.h in Linux kernel source
const FATTR4_FS_LAYOUT_TYPES: u32 = 62;  // Word 1, bit 30
const FATTR4_LAYOUT_TYPES: u32 = 64;      // Word 2, bit 0
const FATTR4_LAYOUT_BLKSIZE: u32 = 65;    // Word 2, bit 1

/// Bitmap of all attributes supported by this server (RFC 5661 Section 5.8)
///
/// Per RFC: SUPPORTED_ATTRS represents filesystem-wide capabilities and should
/// be consistent across all objects (files, directories, pseudo-root).
///
/// This bitmap is used by the client during mount to determine what attributes
/// it can request. Critical attributes include:
/// - MAXREAD/MAXWRITE: Control client rsize/wsize (performance!)
/// - LEASE_TIME: NFSv4.1 lease management
/// - SPACE_*: For df command
/// - FILES_*: For df -i command
const SUPPORTED_ATTRS_BITMAP: u64 = (1u64 << FATTR4_TYPE)
    | (1u64 << FATTR4_FH_EXPIRE_TYPE)
    | (1u64 << FATTR4_SIZE)
    | (1u64 << FATTR4_CHANGE)
    | (1u64 << FATTR4_LINK_SUPPORT)
    | (1u64 << FATTR4_SYMLINK_SUPPORT)
    | (1u64 << FATTR4_FSID)
    | (1u64 << FATTR4_UNIQUE_HANDLES)   // Client FH caching strategy
    | (1u64 << FATTR4_LEASE_TIME)       // CRITICAL for NFSv4.1 leases!
    | (1u64 << FATTR4_ACLSUPPORT)       // ACL capabilities
    | (1u64 << FATTR4_ACL)
    | (1u64 << FATTR4_CANSETTIME)       // Can set timestamps
    | (1u64 << FATTR4_FILEID)
    | (1u64 << FATTR4_FILES_AVAIL)      // For df -i command
    | (1u64 << FATTR4_FILES_FREE)       // For df -i command
    | (1u64 << FATTR4_FILES_TOTAL)      // For df -i command
    | (1u64 << FATTR4_MAXFILESIZE)      // Maximum file size
    | (1u64 << FATTR4_MAXLINK)          // Max hard links
    | (1u64 << FATTR4_MAXNAME)          // Max filename length
    | (1u64 << FATTR4_MAXREAD)          // CRITICAL for client rsize!
    | (1u64 << FATTR4_MAXWRITE)         // CRITICAL for client wsize!
    | (1u64 << FATTR4_MODE)
    | (1u64 << FATTR4_CASE_INSENSITIVE)
    | (1u64 << FATTR4_CASE_PRESERVING)
    | (1u64 << FATTR4_NUMLINKS)
    | (1u64 << FATTR4_OWNER)
    | (1u64 << FATTR4_OWNER_GROUP)
    | (1u64 << FATTR4_RAWDEV)
    | (1u64 << FATTR4_SPACE_AVAIL)      // For df command
    | (1u64 << FATTR4_SPACE_FREE)       // For df command
    | (1u64 << FATTR4_SPACE_TOTAL)      // For df command
    | (1u64 << FATTR4_SPACE_USED)
    | (1u64 << FATTR4_TIME_ACCESS)
    | (1u64 << FATTR4_TIME_METADATA)
    | (1u64 << FATTR4_TIME_MODIFY)
    // The *_SET variants are write-only attrs (SETATTR/createattrs). The
    // Linux client intersects its SETATTR mask with supported_attrs and
    // silently drops what's missing — without these two advertised,
    // utimensat() sends an EMPTY SETATTR and file times never change.
    | (1u64 << FATTR4_TIME_ACCESS_SET)
    | (1u64 << FATTR4_TIME_MODIFY_SET)
    | (1u64 << FATTR4_MOUNTED_ON_FILEID);


/// Encode NFSv4 attributes for pseudo-root (RFC 7530 Section 7)
///
/// Returns (attribute_values, supported_bitmap) with synthetic values
/// Single source of truth for the two pNFS advertisement attributes.
///
/// TWO GETATTR encoders emit these — `encode_attributes_from_snapshot`
/// and the pseudo-root arm in `encode_pseudo_root_attribute` — and the
/// pseudo-root arm is the one a mounting client's fsinfo actually hits
/// (it's what reaches `bl_set_layoutdriver`/`nfs4_set_layoutdriver` on
/// the client). They diverged once: the snapshot arm emitted
/// LAYOUT_BLKSIZE unconditionally while the pseudo-root arm gated it on
/// pNFS being enabled. These helpers exist so the two arms cannot
/// diverge again; per-volume advertisement for the pnfs-block class
/// (docs/plans/pnfs-block-layout-design.md §3/§4a) lands HERE, once.
///
/// We advertise only LAYOUT4_NFSV4_1_FILES (value 1, RFC 8881 §3.3.13).
/// The Linux client picks the *first* type it implements from this
/// list, in array order. FFLv4 (RFC 8435, type 4) needs a different
/// `ff_layout4` body we don't generate — advertising it once made the
/// kernel silently fall back to MDS-direct I/O after parsing a
/// malformed body. Block (RFC 5663/8154/9561, type 3 — NOT 2, the old
/// comment here had OSD2/BLOCK swapped like the LayoutType enum did)
/// requires the extent allocator that doesn't exist yet.
/// `scsi` = the file lives on a pnfs-block volume: advertise
/// LAYOUT4_SCSI (5, RFC 8154/9561 — the type the kernel's v6.11 NVMe
/// support serves) instead of FILES. Per-volume: the client mounts the
/// volume subtree, so the fsinfo that picks its layout driver hits the
/// volume directory and gets that volume's class.
fn encode_fs_layout_types(buf: &mut BytesMut, pnfs_enabled: bool, scsi: bool) -> bool {
    if pnfs_enabled {
        buf.put_u32(1); // array length
        if scsi {
            debug!("  FS_LAYOUT_TYPES (attr 62) → [SCSI]");
            buf.put_u32(5); // LAYOUT4_SCSI
        } else {
            debug!("  FS_LAYOUT_TYPES (attr 62) → [FILES]");
            buf.put_u32(1); // LAYOUT4_NFSV4_1_FILES
        }
        true
    } else {
        debug!("  FS_LAYOUT_TYPES (attr 62) - skipped (pNFS disabled)");
        false
    }
}

/// See `encode_fs_layout_types`. 4 MiB matches the default stripe size
/// for the files class; the scsi class advertises 4 KiB — RFC 8154's
/// wire blksize is the extent-alignment unit, and §8 bounds it ≤ 4 KiB
/// (one commit list can shatter a 4 Mi extent into blksize rows, so
/// the unit is small by design). Gated on pNFS like FS_LAYOUT_TYPES
/// (the previously-divergent arm emitted it unconditionally — unified
/// 2026-08-09, deliberate).
fn encode_layout_blksize(buf: &mut BytesMut, pnfs_enabled: bool, scsi: bool) -> bool {
    if pnfs_enabled {
        let blksize: u32 = if scsi { 4096 } else { 4_194_304 };
        debug!("  LAYOUT_BLKSIZE (attr 65) → {}", blksize);
        buf.put_u32(blksize);
        true
    } else {
        debug!("  LAYOUT_BLKSIZE (attr 65) - skipped (pNFS disabled)");
        false
    }
}

fn encode_pseudo_root_attributes(
    requested_bitmap: &[u32],
    attrs: &crate::nfs::v4::pseudo::PseudoRootAttrs,
    pnfs_enabled: bool,
) -> (Vec<u8>, Vec<u32>) {
    use std::collections::BTreeSet;
    
    
    // Parse bitmap to get list of requested attribute IDs in order
    let mut requested_attrs = BTreeSet::new();
    for (word_idx, &bitmap_word) in requested_bitmap.iter().enumerate() {
        for bit in 0..32 {
            if (bitmap_word & (1 << bit)) != 0 {
                let attr_id = (word_idx * 32 + bit) as u32;
                requested_attrs.insert(attr_id);
            }
        }
    }
    
    debug!("PSEUDO-ROOT GETATTR: Requested attributes: {:?}", requested_attrs);
    
    // Encode attributes in order with SYNTHETIC values
    let mut attr_vals = BytesMut::new();
    let mut supported_attrs = BTreeSet::new();
    
    for attr_id in requested_attrs {
        let before_len = attr_vals.len();
        if encode_pseudo_root_attribute(attr_id, attrs, &mut attr_vals, pnfs_enabled) {
            let after_len = attr_vals.len();
            let bytes_added = after_len - before_len;
            debug!("  Encoded pseudo-root attr {}: {} bytes", attr_id, bytes_added);
            supported_attrs.insert(attr_id);
        }
    }
    
    // Convert supported attributes back to bitmap
    let mut supported_bitmap = vec![0u32; 3];
    for attr_id in supported_attrs {
        let word_idx = (attr_id / 32) as usize;
        let bit = attr_id % 32;
        if word_idx < supported_bitmap.len() {
            supported_bitmap[word_idx] |= 1 << bit;
        }
    }
    
    // Trim trailing zeros from bitmap
    while supported_bitmap.len() > 1 && supported_bitmap.last() == Some(&0) {
        supported_bitmap.pop();
    }
    
    (attr_vals.to_vec(), supported_bitmap)
}

/// Encode attributes for an export entry in pseudo-root READDIR
///
/// Returns ONLY the attributes that the client explicitly requested.
/// This is critical - returning unrequested attributes causes XDR decode errors!
///
/// Uses the snapshot-based approach for consistency with GETATTR.
fn encode_export_entry_attributes(name: &str, requested_attrs: &[u32], pnfs_enabled: bool) -> (Vec<u8>, Vec<u32>) {
    use std::hash::{Hash, Hasher};
    
    // Generate a unique FILEID for this export based on name hash
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut hasher);
    let file_id = hasher.finish() | 0x8000_0000_0000_0000; // Ensure it's high to avoid conflicts
    
    // Create a synthetic snapshot for this export entry
    let now = SystemTime::now();
    let change = now.duration_since(UNIX_EPOCH).unwrap().as_secs();
    
    // CRITICAL: Use SAME FSID as pseudo-root!
    // If we use a different FSID, client thinks it's a different filesystem
    // and expects a mount point, which causes permission issues.
    // Using (0, 0) tells client this is part of the same pseudo-filesystem.
    let snapshot = AttributeSnapshot {
        ftype: 2, // NF4DIR
        size: 4096,
        space_used: 4096,
        fileid: file_id,
        fsid_major: 0, // SAME as pseudo-root!
        fsid_minor: 0, // SAME as pseudo-root!
        atime: now,
        mtime: now,
        ctime: now,
        change,
        mode: 0o755,
        numlinks: 2,
        owner: 0, // root (will be translated to "root" by uid_to_username)
        group: 0, // root (will be translated to "root" by gid_to_groupname)
        path: PathBuf::from(format!("/{}", name)),
    };

    // Use the standard snapshot encoder for consistency
    encode_attributes_from_snapshot(requested_attrs, &snapshot, pnfs_enabled, None)
}

/// Encode attributes from a snapshot (NO VFS I/O)
///
/// This is the RFC-compliant way to encode attributes: all values come from
/// a pre-fetched snapshot, ensuring consistency per RFC 8434 §13.
///
/// Key principle: This function does ZERO I/O, only serialization.
fn encode_attributes_from_snapshot(
    requested_bitmap: &[u32],
    snapshot: &AttributeSnapshot,
    pnfs_enabled: bool,
    // None = files-class (the historical advertisement). Some(minor) =
    // the path lives on a scsi-class volume whose SYNTHETIC fsid minor
    // is `minor` — block volumes are advertised as their OWN filesystem
    // (rig-proven, kernel 7.0): the client reads fs_layout_types ONCE
    // per superblock at its fsinfo probe, so a scsi volume sharing the
    // export root's fsid inherits the root's files-class advertisement
    // and asks for type-1 layouts forever. A distinct fsid makes the
    // volume dir a filesystem crossing; the client probes fsinfo on the
    // volume itself and binds the scsi layout driver for that
    // superblock alone. Free consequence: cross-volume renames/links of
    // scsi files now refuse client-side with EXDEV — the refusal the
    // extents machine wanted anyway (bytes cannot follow a file out of
    // its volume's lvol).
    scsi_fsid: Option<u64>,
) -> (Vec<u8>, Vec<u32>) {
    let scsi = scsi_fsid.is_some();
    use std::collections::BTreeSet;
    
    // Parse bitmap to get list of requested attribute IDs in order
    let mut requested_attrs = BTreeSet::new();
    for (word_idx, &bitmap_word) in requested_bitmap.iter().enumerate() {
        for bit in 0..32 {
            if (bitmap_word & (1 << bit)) != 0 {
                let attr_id = (word_idx * 32 + bit) as u32;
                requested_attrs.insert(attr_id);
            }
        }
    }
    
    debug!("Encoding {} attributes from snapshot", requested_attrs.len());
    
    // Encode attributes in order from snapshot (NO I/O!)
    let mut attr_vals = BytesMut::new();
    let mut supported_attrs = BTreeSet::new();

    // A10: the tier's cached statvfs gauge (relaxed loads, no
    // syscall). Some ⇒ SPACE_*/FILES_* answer with the PVC's real
    // numbers so df reads the disk, not 8 EiB. None (tier off) keeps
    // the historical behavior — a striped pNFS export's capacity is
    // NOT the MDS's local filesystem, so these arms stay silent there.
    let space_view = crate::tier::space::view();

    for attr_id in requested_attrs {
        let before_len = attr_vals.len();
        let encoded = match attr_id {
            FATTR4_FILES_AVAIL if space_view.is_some() => {
                attr_vals.put_u64(space_view.unwrap().files_avail);
                true
            }
            FATTR4_FILES_FREE if space_view.is_some() => {
                attr_vals.put_u64(space_view.unwrap().files_free);
                true
            }
            FATTR4_FILES_TOTAL if space_view.is_some() => {
                attr_vals.put_u64(space_view.unwrap().files_total);
                true
            }
            FATTR4_SPACE_AVAIL if space_view.is_some() => {
                // avail − reserve: what a client write can actually
                // have — matches the admission gate's arithmetic.
                attr_vals.put_u64(space_view.unwrap().avail_bytes);
                true
            }
            FATTR4_SPACE_FREE if space_view.is_some() => {
                attr_vals.put_u64(space_view.unwrap().free_bytes);
                true
            }
            FATTR4_SPACE_TOTAL if space_view.is_some() => {
                attr_vals.put_u64(space_view.unwrap().total_bytes);
                true
            }
            FATTR4_TYPE => {
                attr_vals.put_u32(snapshot.ftype);
                debug!("  Attr {}: TYPE={}", attr_id, snapshot.ftype);
                true
            }
            FATTR4_FH_EXPIRE_TYPE => {
                // FH_EXPIRE_NEVER_EXPIRE (0x00000000) per RFC 7530 Section 5.3
                // Our file handles are persistent and never expire
                attr_vals.put_u32(0);
                debug!("  Attr {}: FH_EXPIRE_TYPE=0 (never expire)", attr_id);
                true
            }
            FATTR4_CHANGE => {
                attr_vals.put_u64(snapshot.change);
                debug!("  Attr {}: CHANGE={}", attr_id, snapshot.change);
                true
            }
            FATTR4_SIZE => {
                attr_vals.put_u64(snapshot.size);
                debug!("  Attr {}: SIZE={}", attr_id, snapshot.size);
                true
            }
            FATTR4_FSID => {
                match scsi_fsid {
                    Some(minor) => {
                        attr_vals.put_u64(SCSI_FSID_MAJOR);
                        attr_vals.put_u64(minor);
                    }
                    None => {
                        attr_vals.put_u64(snapshot.fsid_major);
                        attr_vals.put_u64(snapshot.fsid_minor);
                    }
                }
                true
            }
            FATTR4_RDATTR_ERROR => {
                // No error - snapshot was successful
                attr_vals.put_u32(0); // NFS4_OK
                true
            }
            FATTR4_ACL => {
                // Return empty ACL (use POSIX mode instead)
                // Per RFC 7530 Section 6.4: empty ACL means use MODE attribute
                attr_vals.put_u32(0); // Array length = 0 (no ACEs)
                true
            }
            FATTR4_FILEID => {
                attr_vals.put_u64(snapshot.fileid);
                true
            }
            FATTR4_MOUNTED_ON_FILEID => {
                // Same as FILEID for non-mount points
                attr_vals.put_u64(snapshot.fileid);
                true
            }
            FATTR4_MODE => {
                // NFSv4 MODE = permission bits ONLY (not file type)
                // Per RFC 7530 Section 5.8: mask out file type bits (S_IFMT)
                // Unix mode includes type: 0100644 (file) or 0040755 (dir)
                // NFSv4 MODE should be:   0644 (file) or   0755 (dir)
                let permission_bits = snapshot.mode & 0o7777;
                debug!("  MODE: Unix mode={:o} → NFSv4 permission bits={:o}", snapshot.mode, permission_bits);
                attr_vals.put_u32(permission_bits);
                true
            }
            FATTR4_NUMLINKS => {
                attr_vals.put_u32(snapshot.numlinks);
                true
            }
            FATTR4_OWNER => {
                // Translate UID to username (per RFC 7530 §5.9)
                let owner_str = uid_to_username(snapshot.owner);
                attr_vals.put_u32(owner_str.len() as u32);
                attr_vals.put_slice(owner_str.as_bytes());
                // Pad to 4-byte boundary
                let padding = (4 - (owner_str.len() % 4)) % 4;
                for _ in 0..padding {
                    attr_vals.put_u8(0);
                }
                true
            }
            FATTR4_OWNER_GROUP => {
                // Translate GID to groupname (per RFC 7530 §5.9)
                let group_str = gid_to_groupname(snapshot.group);
                attr_vals.put_u32(group_str.len() as u32);
                attr_vals.put_slice(group_str.as_bytes());
                // Pad to 4-byte boundary
                let padding = (4 - (group_str.len() % 4)) % 4;
                for _ in 0..padding {
                    attr_vals.put_u8(0);
                }
                true
            }
            FATTR4_SPACE_USED => {
                attr_vals.put_u64(snapshot.space_used);
                true
            }
            FATTR4_TIME_ACCESS => {
                let duration = snapshot.atime.duration_since(UNIX_EPOCH).unwrap();
                attr_vals.put_i64(duration.as_secs() as i64);
                attr_vals.put_u32(duration.subsec_nanos());
                true
            }
            FATTR4_TIME_METADATA => {
                let duration = snapshot.ctime.duration_since(UNIX_EPOCH).unwrap();
                let secs = duration.as_secs() as i64;
                let nsecs = duration.subsec_nanos();
                attr_vals.put_i64(secs);
                attr_vals.put_u32(nsecs);
                debug!("  Attr {}: TIME_METADATA={}.{:09} (i64+u32)", attr_id, secs, nsecs);
                true
            }
            FATTR4_TIME_MODIFY => {
                let duration = snapshot.mtime.duration_since(UNIX_EPOCH).unwrap();
                let secs = duration.as_secs() as i64;
                let nsecs = duration.subsec_nanos();
                attr_vals.put_i64(secs);
                attr_vals.put_u32(nsecs);
                debug!("  Attr {}: TIME_MODIFY={}.{:09} (i64+u32)", attr_id, secs, nsecs);
                true
            }
            FATTR4_SUPPORTED_ATTRS => {
                // RFC 8881 Section 3.3.1 - bitmap4 variable-length array
                let supported = SUPPORTED_ATTRS_BITMAP;
                let word0 = (supported & 0xFFFFFFFF) as u32;
                let mut word1 = (supported >> 32) as u32;
                // Word 2 always carries CHANGE_ATTR_TYPE (attr 79): without
                // it the client cannot ORDER attr replies by the change
                // value — an out-of-order GETATTR reply with a stale size
                // is applied newest-received (F14's visible half).
                let mut word2 = 1u32 << (FATTR4_CHANGE_ATTR_TYPE % 32);

                // Only advertise pNFS attributes if pNFS is enabled
                if pnfs_enabled {
                    // pNFS attributes (Linux kernel numbering)
                    word1 |= 1 << (62 % 32);  // FS_LAYOUT_TYPES (attr 62, word 1, bit 30)
                    word2 |= 1 << (65 % 32);  // LAYOUT_BLKSIZE (attr 65, word 2, bit 1)
                }

                attr_vals.put_u32(3); // array length
                attr_vals.put_u32(word0);
                attr_vals.put_u32(word1);
                attr_vals.put_u32(word2);
                debug!("  SUPPORTED_ATTRS: 3 words [0x{:08x}, 0x{:08x}, 0x{:08x}] (pnfs={})", word0, word1, word2, pnfs_enabled);
                true
            }
            FATTR4_CHANGE_ATTR_TYPE => {
                // NFS4_CHANGE_TYPE_IS_MONOTONIC_INCR: the change_counter
                // guarantees strictly increasing values per mutation, so
                // the client may discard replies whose change is older.
                attr_vals.put_u32(0);
                debug!("  Attr {}: CHANGE_ATTR_TYPE=MONOTONIC_INCR", attr_id);
                true
            }
            FATTR4_MAXREAD => {
                // Maximum read size (1MB)
                attr_vals.put_u64(1024 * 1024);
                true
            }
            FATTR4_MAXWRITE => {
                // Maximum write size (1MB)
                attr_vals.put_u64(1024 * 1024);
                true
            }
            FATTR4_MAXNAME => {
                // Maximum filename length
                attr_vals.put_u32(255);
                true
            }
            FATTR4_MAXLINK => {
                // Maximum hard links
                attr_vals.put_u32(65535);
                true
            }
            FATTR4_CANSETTIME => {
                // Server can set time
                attr_vals.put_u32(1); // TRUE
                true
            }
            FATTR4_CASE_INSENSITIVE => {
                // Filesystem is case sensitive
                attr_vals.put_u32(0); // FALSE
                true
            }
            FATTR4_CASE_PRESERVING => {
                // Filesystem preserves case
                attr_vals.put_u32(1); // TRUE
                true
            }
            FATTR4_LINK_SUPPORT => {
                // Supports hard links
                attr_vals.put_u32(1); // TRUE
                true
            }
            FATTR4_SYMLINK_SUPPORT => {
                // Supports symbolic links
                attr_vals.put_u32(1); // TRUE
                true
            }
            FATTR4_UNIQUE_HANDLES => {
                // File handles are unique
                attr_vals.put_u32(1); // TRUE
                true
            }
            FATTR4_LEASE_TIME => {
                // Lease time in seconds
                attr_vals.put_u32(90);
                true
            }
            FATTR4_SUPPATTR_EXCLCREAT => {
                // Attributes supported for exclusive create
                // Return minimal set: TYPE, MODE
                let supported: u64 = (1u64 << FATTR4_TYPE) | (1u64 << FATTR4_MODE);
                attr_vals.put_u32(2); // 2 words
                attr_vals.put_u32((supported & 0xFFFFFFFF) as u32);
                attr_vals.put_u32((supported >> 32) as u32);
                true
            }
            FATTR4_FS_LAYOUT_TYPES => encode_fs_layout_types(&mut attr_vals, pnfs_enabled, scsi),
            FATTR4_LAYOUT_BLKSIZE => encode_layout_blksize(&mut attr_vals, pnfs_enabled, scsi),
            _ => {
                debug!("  Attribute {} not supported in snapshot encoder", attr_id);
                false
            }
        };
        
        if encoded {
            let bytes_added = attr_vals.len() - before_len;
            debug!("    → Encoded {} bytes for attr {}", bytes_added, attr_id);
            supported_attrs.insert(attr_id);
        } else {
            debug!("    → Attr {} not encoded (unsupported)", attr_id);
        }
    }
    
    debug!("=== Attribute encoding complete ===");
    debug!("  Total attributes encoded: {}", supported_attrs.len());
    debug!("  Total bytes: {}", attr_vals.len());
    debug!("  Encoded attr IDs: {:?}", supported_attrs.iter().collect::<Vec<_>>());
    
    // Convert supported attributes back to bitmap
    let mut supported_bitmap = vec![0u32; 3];
    for attr_id in supported_attrs {
        let word_idx = (attr_id / 32) as usize;
        let bit = attr_id % 32;
        if word_idx < supported_bitmap.len() {
            supported_bitmap[word_idx] |= 1 << bit;
        }
    }
    
    // Trim trailing zeros from bitmap
    while supported_bitmap.len() > 1 && supported_bitmap.last() == Some(&0) {
        supported_bitmap.pop();
    }
    
    debug!("Encoded {} bytes from snapshot", attr_vals.len());
    
    (attr_vals.to_vec(), supported_bitmap)
}

/// Encode a single pseudo-root attribute
fn encode_pseudo_root_attribute(
    attr_id: u32,
    attrs: &crate::nfs::v4::pseudo::PseudoRootAttrs,
    buf: &mut BytesMut,
    pnfs_enabled: bool,
) -> bool {
    use crate::nfs::v4::pseudo::{PSEUDO_ROOT_FSID, PSEUDO_ROOT_FILEID};
    
    match attr_id {
        FATTR4_TYPE => {
            buf.put_u32(2); // NF4DIR - directory
            true
        }
        FATTR4_FSID => {
            // Pseudo-filesystem FSID: {0, 0}
            buf.put_u64(PSEUDO_ROOT_FSID.0);
            buf.put_u64(PSEUDO_ROOT_FSID.1);
            true
        }
        FATTR4_FILEID => {
            // Pseudo-root file ID: 1
            buf.put_u64(PSEUDO_ROOT_FILEID);
            true
        }
        FATTR4_MOUNTED_ON_FILEID => {
            // Same as FILEID for pseudo-root
            buf.put_u64(PSEUDO_ROOT_FILEID);
            true
        }
        FATTR4_SIZE => {
            buf.put_u64(attrs.size); // Synthetic size (4096)
            true
        }
        FATTR4_NUMLINKS => {
            buf.put_u32(attrs.nlink); // 2 + number of exports
            true
        }
        FATTR4_MODE => {
            buf.put_u32(0o755); // rwxr-xr-x
            true
        }
        FATTR4_CHANGE => {
            buf.put_u64(attrs.create_time);
            true
        }
        FATTR4_TIME_ACCESS | FATTR4_TIME_METADATA | FATTR4_TIME_MODIFY => {
            // All times = pseudo-root creation time
            buf.put_i64(attrs.create_time as i64); // seconds
            buf.put_u32(0); // nanoseconds
            true
        }
        FATTR4_OWNER => {
            // "root"
            buf.put_u32(4);
            buf.put_slice(b"root");
            true
        }
        FATTR4_OWNER_GROUP => {
            // "root"
            buf.put_u32(4);
            buf.put_slice(b"root");
            true
        }
        FATTR4_RAWDEV => {
            // Raw device specdata4 (major, minor) - pseudo-root is not a device
            buf.put_u32(0); // major
            buf.put_u32(0); // minor
            true
        }
        FATTR4_SPACE_USED => {
            // Space used by pseudo-root (minimal)
            buf.put_u64(4096); // One block
            true
        }
        FATTR4_SPACE_AVAIL | FATTR4_SPACE_FREE | FATTR4_SPACE_TOTAL => {
            // A10: when the tier's space gauge is live, df must read
            // the PVC even through the pseudo-root FH (the mount root
            // is where statfs probes land). Tier off keeps the
            // historical "infinite" answer.
            match crate::tier::space::view() {
                Some(v) => buf.put_u64(match attr_id {
                    FATTR4_SPACE_AVAIL => v.avail_bytes,
                    FATTR4_SPACE_FREE => v.free_bytes,
                    _ => v.total_bytes,
                }),
                // Pseudo-filesystem has "infinite" space
                None => buf.put_u64(u64::MAX / 2), // Very large but not overflow
            }
            true
        }
        FATTR4_SUPPORTED_ATTRS => {
            // RFC 8881 Section 3.3.1 - bitmap4 is variable-length array of u32
            let supported = SUPPORTED_ATTRS_BITMAP;
            let word0 = (supported & 0xFFFFFFFF) as u32;
            let mut word1 = (supported >> 32) as u32;
            // Attr 79 always advertised — see the file-attr arm (F14).
            let mut word2 = 1u32 << (FATTR4_CHANGE_ATTR_TYPE % 32);

            // Only advertise pNFS attributes if pNFS is enabled
            if pnfs_enabled {
                // pNFS attributes (Linux kernel numbering)
                word1 |= 1 << (62 % 32);  // FS_LAYOUT_TYPES (attr 62, word 1, bit 30)
                word2 |= 1 << (65 % 32);  // LAYOUT_BLKSIZE (attr 65, word 2, bit 1)
            }

            buf.put_u32(3); // array length: 3 words
            buf.put_u32(word0);   // word 0 (attrs 0-31)
            buf.put_u32(word1);   // word 1 (attrs 32-63)
            buf.put_u32(word2);   // word 2 (attrs 64-95)
            debug!("  SUPPORTED_ATTRS (pseudo-root): 3 words [0x{:08x}, 0x{:08x}, 0x{:08x}] (pnfs={})", word0, word1, word2, pnfs_enabled);
            true
        }
        FATTR4_CHANGE_ATTR_TYPE => {
            // The server-capabilities GETATTR arrives on the pseudo-root
            // filehandle — this arm is what actually reaches the client's
            // change_attr_type. MONOTONIC_INCR (change_counter, F14).
            buf.put_u32(0);
            true
        }
        FATTR4_FS_LAYOUT_TYPES => encode_fs_layout_types(buf, pnfs_enabled, false),
        FATTR4_LAYOUT_BLKSIZE => encode_layout_blksize(buf, pnfs_enabled, false),
        _ => {
            // Attribute not supported for pseudo-root
            debug!("  Pseudo-root attr {} not supported", attr_id);
            false
        }
    }
}


/// File operation handler
pub struct FileOperationHandler {
    fh_mgr: Arc<FileHandleManager>,
    /// Whether pNFS support is enabled (affects advertised attributes)
    pnfs_enabled: bool,
    /// View over the io handler's open-fd cache (F17b): lets GETATTR
    /// serve a renamed-over/removed file that is still open server-side
    /// via fstat instead of STALE. None only in unit tests constructed
    /// without an io handler.
    open_files: Option<crate::nfs::v4::operations::ioops::OpenFileView>,
    /// Present only in the MDS role. Used to answer SPACE_USED honestly
    /// for striped files — see [`Self::correct_space_used`].
    pnfs_handler: Option<Arc<dyn crate::pnfs::PnfsOperations>>,
}

impl FileOperationHandler {
    /// Create a new file operation handler
    /// Whether `path` (absolute, under the export) lives on a
    /// scsi-class (pnfs-block) volume — and if so, the volume's
    /// synthetic fsid minor (see `encode_attributes_from_snapshot`'s
    /// `scsi_fsid` doc for why the class rides on the fsid). Every
    /// "don't know" — no pNFS handler, path outside the export, empty
    /// key — answers None, keeping the historical files-class
    /// advertisement untouched.
    fn scsi_fsid_for_path(&self, path: &std::path::Path) -> Option<u64> {
        let p = self.pnfs_handler.as_ref()?;
        let export = self.fh_mgr.get_export_path().to_path_buf();
        let key = path
            .strip_prefix(&export)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned();
        if key.is_empty() {
            return None;
        }
        if p.layout_class_for(&key) != crate::pnfs::mds::layout::LayoutClass::Scsi {
            return None;
        }
        let volume = key.split('/').find(|c| !c.is_empty())?;
        Some(block_volume_fsid_minor(volume))
    }

    pub fn new(fh_mgr: Arc<FileHandleManager>, pnfs_enabled: bool) -> Self {
        Self { fh_mgr, pnfs_enabled, open_files: None, pnfs_handler: None }
    }

    /// Attach the pNFS handler (MDS role only).
    pub fn with_pnfs_handler(
        mut self,
        pnfs: Option<Arc<dyn crate::pnfs::PnfsOperations>>,
    ) -> Self {
        self.pnfs_handler = pnfs;
        self
    }

    /// Report a striped file's allocation as its size instead of the MDS
    /// stub's block count.
    ///
    /// For a placement-pinned file the MDS's local file is created with
    /// `set_len` and never written, so `blocks()` is 0 while `len()` is
    /// the real size — the exact metadata signature of a fully sparse
    /// file. The bytes are not missing; they are on the DS fleet.
    ///
    /// Measured consequence of reporting the raw 0 (lima, 2026-08-01):
    /// `tar --sparse` of a 24 MiB striped file produced a 10,240-byte
    /// archive and restored a file containing ZERO non-zero bytes, exit
    /// status 0. `du` reported 0. sparse-aware tools trust `st_blocks`
    /// to mean "this range is a hole" and skip reading it, so a backup
    /// silently contains nothing. (`cp --sparse=auto/always` was
    /// verified to still copy correctly — it reads the data.)
    ///
    /// `size` is an over-estimate for a striped file that is genuinely
    /// sparse on the DSes. That direction is deliberate: over-reporting
    /// allocation makes tools do more work, under-reporting it to zero
    /// makes them skip real data. Summing true DS allocation would need
    /// a fan-out on every GETATTR and has no answer while a DS is down.
    fn correct_space_used(&self, snapshot: &mut AttributeSnapshot) {
        let Some(pnfs) = &self.pnfs_handler else {
            return;
        };
        if snapshot.space_used != 0 || snapshot.size == 0 {
            return;
        }
        let export = self.fh_mgr.get_export_path();
        let key = snapshot
            .path
            .strip_prefix(export)
            .unwrap_or(&snapshot.path)
            .to_string_lossy()
            .into_owned();
        if !key.is_empty() && pnfs.is_pnfs_managed(&key) {
            snapshot.space_used = snapshot.size;
        }
    }

    /// Attach the io handler's open-file view (dispatcher wiring).
    pub fn with_open_files(
        mut self,
        view: crate::nfs::v4::operations::ioops::OpenFileView,
    ) -> Self {
        self.open_files = Some(view);
        self
    }

    /// Borrow the underlying file-handle manager. The dispatcher uses
    /// this to resolve filehandles in operations (LAYOUTCOMMIT) that
    /// don't go through a `FileOperationHandler` method but still need
    /// to walk the FH→path mapping the manager owns.
    pub fn fh_manager(&self) -> &Arc<FileHandleManager> {
        &self.fh_mgr
    }

    /// Handle PUTROOTFH operation
    /// 
    /// RFC 5661 allows optimization: if server has a single export, it can
    /// return that export's root directly instead of the pseudo-root.
    /// This enables "direct mount" (Option B) for CSI/single-export scenarios.
    pub fn handle_putrootfh(
        &self,
        _op: PutRootFhOp,
        ctx: &mut CompoundContext,
    ) -> PutRootFhRes {
        debug!("📁 PUTROOTFH - Determining root filehandle to return");
        debug!("   Previous current_fh: {:?}", ctx.current_fh.as_ref().map(|fh| fh.data.len()));

        // Check if we have a single export (CSI/direct mount scenario)
        let exports = self.fh_mgr.get_pseudo_fs().list_exports();
        debug!("   Server has {} export(s): {:?}", exports.len(), exports);

        if exports.len() == 1 {
            // OPTION B: Single export - return export root directly (RFC 5661 optimization)
            let export_name = &exports[0];
            debug!("   🎯 Single export detected: '{}'", export_name);
            debug!("   → Using OPTION B: Direct export mount (bypass pseudo-root)");
            
            match self.fh_mgr.lookup_export(export_name) {
                Some(export) => {
                    debug!("   Export found: path={:?}", export.path);
                    
                    // Get filehandle for the actual export directory
                    match self.fh_mgr.get_or_create_handle(&export.path) {
                        Ok(fh) => {
                            debug!("   ✅ Returning EXPORT ROOT directly: {} bytes", fh.data.len());
                            debug!("   Export FH (hex): {:02x?}", &fh.data[0..std::cmp::min(20, fh.data.len())]);
                            debug!("   → Client can now access files directly without LOOKUP");
                            ctx.current_fh = Some(fh);
                            return PutRootFhRes {
                                status: Nfs4Status::Ok,
                            };
                        }
                        Err(e) => {
                            warn!("   ❌ Failed to create handle for export root: {}", e);
                            warn!("   → Falling back to pseudo-root");
                        }
                    }
                }
                None => {
                    warn!("   ⚠️ Export '{}' not found in registry", export_name);
                    warn!("   → Falling back to pseudo-root");
                }
            }
        } else if exports.is_empty() {
            warn!("   ⚠️ No exports configured!");
        } else {
            // OPTION A: Multiple exports - use pseudo-root for browsing
            debug!("   🌳 Multiple exports detected: {:?}", exports);
            debug!("   → Using OPTION A: Pseudo-root with browsing/discovery");
        }

        // Get pseudo-root filehandle (RFC 7530 Section 7)
        match self.fh_mgr.get_root_fh() {
            Ok(fh) => {
                debug!("   ✅ Returning PSEUDO-ROOT: {} bytes", fh.data.len());
                debug!("   Pseudo-root FH (hex): {:02x?}", &fh.data[0..std::cmp::min(20, fh.data.len())]);
                debug!("   → Client will need LOOKUP to traverse to exports");
                ctx.current_fh = Some(fh);
                PutRootFhRes {
                    status: Nfs4Status::Ok,
                }
            }
            Err(e) => {
                warn!("❌ PUTROOTFH failed: {}", e);
                PutRootFhRes {
                    status: Nfs4Status::Resource,
                }
            }
        }
    }

    /// Handle PUTFH operation
    pub fn handle_putfh(
        &self,
        op: PutFhOp,
        ctx: &mut CompoundContext,
    ) -> PutFhRes {
        debug!("PUTFH");

        // Validate filehandle
        match self.fh_mgr.validate_handle(&op.filehandle) {
            Ok(_) => {
                ctx.current_fh = Some(op.filehandle);
                PutFhRes {
                    status: Nfs4Status::Ok,
                }
            }
            // Another incarnation's handle → STALE, which kernel clients
            // recover from by re-walking the path. BADHANDLE here instead
            // wedges the mount permanently (clients treat it as fatal).
            Err(crate::nfs::v4::filehandle::HandleError::Stale) => {
                warn!("PUTFH: stale handle (other server incarnation) — answering NFS4ERR_STALE");
                PutFhRes {
                    status: Nfs4Status::Stale,
                }
            }
            Err(e) => {
                warn!("PUTFH validation failed: {}", e);
                PutFhRes {
                    status: Nfs4Status::BadHandle,
                }
            }
        }
    }

    /// Handle GETFH operation
    pub fn handle_getfh(
        &self,
        _op: GetFhOp,
        ctx: &CompoundContext,
    ) -> GetFhRes {
        debug!("GETFH");

        if let Some(ref fh) = ctx.current_fh {
            GetFhRes {
                status: Nfs4Status::Ok,
                filehandle: Some(fh.clone()),
            }
        } else {
            GetFhRes {
                status: Nfs4Status::NoFileHandle,
                filehandle: None,
            }
        }
    }

    /// Handle SAVEFH operation
    pub fn handle_savefh(
        &self,
        _op: SaveFhOp,
        ctx: &mut CompoundContext,
    ) -> SaveFhRes {
        debug!("SAVEFH");

        if let Some(ref fh) = ctx.current_fh {
            ctx.saved_fh = Some(fh.clone());
            SaveFhRes {
                status: Nfs4Status::Ok,
            }
        } else {
            SaveFhRes {
                status: Nfs4Status::NoFileHandle,
            }
        }
    }

    /// Handle RESTOREFH operation
    pub fn handle_restorefh(
        &self,
        _op: RestoreFhOp,
        ctx: &mut CompoundContext,
    ) -> RestoreFhRes {
        debug!("RESTOREFH");

        if let Some(ref fh) = ctx.saved_fh {
            ctx.current_fh = Some(fh.clone());
            RestoreFhRes {
                status: Nfs4Status::Ok,
            }
        } else {
            RestoreFhRes {
                status: Nfs4Status::RestoReFh,
            }
        }
    }

    /// Handle LOOKUP operation
    pub async fn handle_lookup(
        &self,
        op: LookupOp,
        ctx: &mut CompoundContext,
    ) -> LookupRes {
        debug!("🔍 LOOKUP called: component='{}'", op.component);
        debug!("   Component length: {} bytes", op.component.len());
        debug!("   Component bytes (hex): {:02x?}", op.component.as_bytes());

        if let Some(status) = validate_component_name(&op.component) {
            warn!("LOOKUP: invalid component name → {:?}", status);
            return LookupRes { status };
        }

        // Check current filehandle
        let current_fh = match &ctx.current_fh {
            Some(fh) => fh,
            None => {
                warn!("❌ LOOKUP: No current filehandle!");
                return LookupRes {
                    status: Nfs4Status::NoFileHandle,
                };
            }
        };

        let is_pseudo = self.fh_mgr.is_pseudo_root(current_fh);
        debug!("   Current FH: {} bytes, is_pseudo_root={}", current_fh.data.len(), is_pseudo);
        debug!("   Current FH (hex): {:02x?}", &current_fh.data[0..std::cmp::min(20, current_fh.data.len())]);

        // Special case: LOOKUP "." returns current filehandle unchanged
        if op.component == "." {
            debug!("✅ LOOKUP '.': returning current filehandle (no change)");
            return LookupRes {
                status: Nfs4Status::Ok,
            };
        }

        // Special case: LOOKUP ".." from pseudo-root is not allowed
        if op.component == ".." && is_pseudo {
            info!("❌ LOOKUP '..': Cannot go above pseudo-root");
            return LookupRes {
                status: Nfs4Status::NoEnt,
            };
        }

        // Handle LOOKUP from pseudo-root (RFC 7530 Section 7)
        if is_pseudo {
            debug!("🔍 LOOKUP from PSEUDO-ROOT: component='{}' (looking for export)", op.component);
            
            // Lookup export by name
            if let Some(export) = self.fh_mgr.lookup_export(&op.component) {
                debug!("✅ Found export '{}' → path {:?}", export.name, export.path);
                
                // Verify the export path exists
                match tokio::fs::metadata(&export.path).await {
                    Ok(metadata) => {
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::MetadataExt;
                            debug!("   Export metadata: is_dir={}, mode={:o}", 
                                  metadata.is_dir(), metadata.mode());
                        }
                        #[cfg(not(unix))]
                        {
                            debug!("   Export metadata: is_dir={}", metadata.is_dir());
                        }
                    }
                    Err(e) => {
                        warn!("   Export path does not exist: {}", e);
                        return LookupRes {
                            status: Nfs4Status::NoEnt,
                        };
                    }
                }
                
                // Create filehandle for the export's actual path
                match self.fh_mgr.get_or_create_handle(&export.path) {
                    Ok(fh) => {
                        debug!("   Created filehandle: {} bytes", fh.data.len());
                        ctx.current_fh = Some(fh);
                        return LookupRes {
                            status: Nfs4Status::Ok,
                        };
                    }
                    Err(e) => {
                        warn!("LOOKUP: Failed to create handle for export: {}", e);
                        return LookupRes {
                            status: Nfs4Status::Resource,
                        };
                    }
                }
            } else {
                warn!("❌ Export '{}' not found in pseudo-filesystem", op.component);
                let available = self.fh_mgr.get_pseudo_fs().list_exports();
                warn!("   Available exports: {:?}", available);
                return LookupRes {
                    status: Nfs4Status::NoEnt,
                };
            }
        }

        // Regular filesystem LOOKUP
        let current_path = match self.fh_mgr.resolve_handle(current_fh) {
            Ok(path) => path,
            Err(e) => {
                warn!("LOOKUP: Failed to resolve handle: {}", e);
                return LookupRes {
                    status: Nfs4Status::Stale,
                };
            }
        };

        // Build target path
        let target_path = if op.component == ".." {
            // Special handling for parent directory
            match current_path.parent() {
                Some(parent) => parent.to_path_buf(),
                None => {
                    // Already at root
                    debug!("LOOKUP '..': Already at filesystem root");
                    return LookupRes {
                        status: Nfs4Status::NoEnt,
                    };
                }
            }
        } else {
            current_path.join(&op.component)
        };

        // Check if the target path exists.
        //
        // **Use `symlink_metadata` (not `metadata`) so we don't
        // dereference a trailing symlink.** RFC 5661 §16.10.5: LOOKUP
        // returns the filehandle of the named object, even if that
        // object is a symbolic link — the client follows it via
        // READLINK if it wants to. Following at LOOKUP time is wrong
        // and breaks any path with a dangling symlink as a leaf
        // (which pynfs's --maketree creates explicitly to test the
        // SYMLINK error class — st_lookupp / st_putfh / st_rename
        // tests all expected NOENT vs SYMLINK splits hinge on this).
        let metadata = match tokio::fs::symlink_metadata(&target_path).await {
            Ok(m) => m,
            Err(e) => {
                debug!("LOOKUP: Path {:?} does not exist: {}", target_path, e);
                return LookupRes {
                    status: if e.kind() == std::io::ErrorKind::NotFound {
                        Nfs4Status::NoEnt
                    } else if e.kind() == std::io::ErrorKind::PermissionDenied {
                        Nfs4Status::Access
                    } else {
                        Nfs4Status::Io
                    },
                };
            }
        };

        debug!("LOOKUP: Found {:?} (is_dir={}, is_file={})", 
               target_path, metadata.is_dir(), metadata.is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            debug!("   → Metadata: mode={:o}, uid={}, gid={}, ino={}", 
                   metadata.mode(), metadata.uid(), metadata.gid(), metadata.ino());
        }

        // Generate filehandle for target
        match self.fh_mgr.get_or_create_handle(&target_path) {
            Ok(fh) => {
                debug!("✅ LOOKUP succeeded: '{}' → FH {} bytes", op.component, fh.data.len());
                debug!("   New current FH (hex): {:02x?}", &fh.data[0..std::cmp::min(20, fh.data.len())]);
                ctx.current_fh = Some(fh);
                LookupRes {
                    status: Nfs4Status::Ok,
                }
            }
            Err(e) => {
                warn!("❌ LOOKUP: Failed to create handle: {}", e);
                LookupRes {
                    status: Nfs4Status::Resource,
                }
            }
        }
    }

    /// Handle LOOKUPP operation
    pub async fn handle_lookupp(
        &self,
        _op: LookupPOp,
        ctx: &mut CompoundContext,
    ) -> LookupPRes {
        debug!("LOOKUPP");

        // Check current filehandle
        let current_fh = match &ctx.current_fh {
            Some(fh) => fh,
            None => {
                return LookupPRes {
                    status: Nfs4Status::NoFileHandle,
                };
            }
        };

        // Cannot go up from pseudo-root (RFC 7530 Section 7)
        if self.fh_mgr.is_pseudo_root(current_fh) {
            debug!("LOOKUPP: Cannot go above pseudo-root");
            return LookupPRes {
                status: Nfs4Status::NoEnt,
            };
        }

        // Resolve current path
        let current_path = match self.fh_mgr.resolve_handle(current_fh) {
            Ok(path) => path,
            Err(e) => {
                warn!("LOOKUPP: Failed to resolve handle: {}", e);
                return LookupPRes {
                    status: Nfs4Status::Stale,
                };
            }
        };

        // RFC 5661 §18.10: LOOKUPP requires the current filehandle to be a
        // directory. A non-directory CFH MUST return NFS4ERR_NOTDIR; a
        // symlink CFH MUST return NFS4ERR_SYMLINK (so the client knows it
        // can READLINK to follow it). Use symlink_metadata() so we don't
        // dereference symlinks at this step.
        match current_path.symlink_metadata() {
            Ok(m) if m.is_symlink() => {
                return LookupPRes { status: Nfs4Status::SymLink };
            }
            Ok(m) if !m.is_dir() => {
                return LookupPRes { status: Nfs4Status::NotDir };
            }
            Ok(_) => { /* directory — proceed */ }
            Err(_) => {
                return LookupPRes { status: Nfs4Status::Stale };
            }
        }

        // Get parent
        let parent_path = match current_path.parent() {
            Some(p) => p.to_path_buf(),
            None => {
                // Already at root
                return LookupPRes {
                    status: Nfs4Status::NoEnt,
                };
            }
        };

        // Check if we're trying to go above the export root
        // Compare with the export root from the file handle manager
        let export_root = self.fh_mgr.get_export_path();
        if !parent_path.starts_with(export_root) {
            debug!("LOOKUPP: Attempt to go above export root (current={:?}, parent={:?}, export={:?})",
                   current_path, parent_path, export_root);
            return LookupPRes {
                status: Nfs4Status::NoEnt,
            };
        }

        // Check if the parent path exists
        let metadata = match tokio::fs::metadata(&parent_path).await {
            Ok(m) => m,
            Err(e) => {
                debug!("LOOKUPP: Parent path {:?} does not exist: {}", parent_path, e);
                return LookupPRes {
                    status: if e.kind() == std::io::ErrorKind::NotFound {
                        Nfs4Status::NoEnt
                    } else if e.kind() == std::io::ErrorKind::PermissionDenied {
                        Nfs4Status::Access
                    } else {
                        Nfs4Status::Io
                    },
                };
            }
        };

        // Verify it's a directory
        if !metadata.is_dir() {
            warn!("LOOKUPP: Parent path {:?} is not a directory", parent_path);
            return LookupPRes {
                status: Nfs4Status::NotDir,
            };
        }

        debug!("LOOKUPP: Moving from {:?} to parent {:?}", current_path, parent_path);

        // Generate filehandle for parent
        match self.fh_mgr.get_or_create_handle(&parent_path) {
            Ok(fh) => {
                ctx.current_fh = Some(fh);
                LookupPRes {
                    status: Nfs4Status::Ok,
                }
            }
            Err(e) => {
                warn!("LOOKUPP: Failed to create handle: {}", e);
                LookupPRes {
                    status: Nfs4Status::Resource,
                }
            }
        }
    }

    /// Handle ACCESS operation
    pub async fn handle_access(
        &self,
        op: AccessOp,
        ctx: &CompoundContext,
    ) -> AccessRes {
        debug!("🔐 ACCESS called: mask=0x{:02x}", op.access);
        debug!("   Requested: READ={}, LOOKUP={}, MODIFY={}, EXTEND={}, DELETE={}, EXECUTE={}",
              op.access & ACCESS4_READ != 0,
              op.access & ACCESS4_LOOKUP != 0,
              op.access & ACCESS4_MODIFY != 0,
              op.access & ACCESS4_EXTEND != 0,
              op.access & ACCESS4_DELETE != 0,
              op.access & ACCESS4_EXECUTE != 0);

        // Check current filehandle
        let current_fh = match &ctx.current_fh {
            Some(fh) => fh,
            None => {
                warn!("ACCESS: No current filehandle!");
                return AccessRes {
                    status: Nfs4Status::NoFileHandle,
                    supported: 0,
                    access: 0,
                };
            }
        };

        let is_pseudo = self.fh_mgr.is_pseudo_root(current_fh);
        debug!("   Current FH: {} bytes, is_pseudo_root={}", current_fh.data.len(), is_pseudo);

        // Pseudo-root is always accessible for READ and LOOKUP
        if is_pseudo {
            let supported = ACCESS4_READ | ACCESS4_LOOKUP | ACCESS4_EXECUTE;
            debug!("✅ ACCESS on PSEUDO-ROOT - granting: READ | LOOKUP | EXECUTE (mask=0x{:02x})", supported);
            return AccessRes {
                status: Nfs4Status::Ok,
                supported,
                access: op.access & supported,
            };
        }

        // Check if this is a directory (need EXECUTE for traversal)
        // Per nfs_execute_ok() in Linux kernel: directories need execute permission
        let path = match self.fh_mgr.resolve_handle(current_fh) {
            Ok(p) => p,
            Err(_) => {
                // Fallback: grant what was requested
                let supported = ACCESS4_READ | ACCESS4_LOOKUP | ACCESS4_MODIFY |
                               ACCESS4_EXTEND | ACCESS4_DELETE | ACCESS4_EXECUTE;
                return AccessRes {
                    status: Nfs4Status::Ok,
                    supported,
                    access: op.access & supported,
                };
            }
        };

        let supported = ACCESS4_READ | ACCESS4_LOOKUP | ACCESS4_MODIFY |
                       ACCESS4_EXTEND | ACCESS4_DELETE | ACCESS4_EXECUTE;
        let mut granted = op.access & supported;

        // CRITICAL: Directories always need EXECUTE permission for VFS traversal
        // Even if client doesn't request it, VFS will check MAY_EXEC later
        if let Ok(metadata) = tokio::fs::metadata(&path).await {
            if metadata.is_dir() {
                granted |= ACCESS4_EXECUTE;
                debug!("   → Directory: always granting EXECUTE for VFS traversal");
            }
        }

        debug!("✅ ACCESS on REGULAR FILE/DIR - granting: mask=0x{:02x}", granted);
        debug!("   READ={}, LOOKUP={}, MODIFY={}, EXTEND={}, DELETE={}, EXECUTE={}",
               granted & ACCESS4_READ != 0,
               granted & ACCESS4_LOOKUP != 0,
               granted & ACCESS4_MODIFY != 0,
               granted & ACCESS4_EXTEND != 0,
               granted & ACCESS4_DELETE != 0,
               granted & ACCESS4_EXECUTE != 0);

        AccessRes {
            status: Nfs4Status::Ok,
            supported,
            access: granted,
        }
    }

    /// Handle GETATTR operation
    pub async fn handle_getattr(
        &self,
        op: GetAttrOp,
        ctx: &CompoundContext,
    ) -> GetAttrRes {
        debug!("GETATTR: attrs={:?}", op.attr_request);

        // Check current filehandle
        let current_fh = match &ctx.current_fh {
            Some(fh) => fh,
            None => {
                return GetAttrRes {
                    status: Nfs4Status::NoFileHandle,
                    obj_attributes: None,
                };
            }
        };

        // Check if this is the pseudo-root (RFC 7530 Section 7)
        if self.fh_mgr.is_pseudo_root(current_fh) {
            debug!("📂 GETATTR for PSEUDO-ROOT (synthetic attributes)");
            return self.handle_pseudo_root_getattr(op).await;
        }

        // Resolve path. A stale resolve (renamed-over/removed object)
        // still answers through a server-held open fd when one exists —
        // POSIX unlink-open semantics (F17b). This is what stops the
        // kernel client's fileid-staleness recovery cycling after every
        // postgres rename-over: attributes keep coming from the ORIGINAL
        // inode for as long as it is open, with the original fileid and
        // a consistent change counter.
        let path = match self.fh_mgr.resolve_handle(current_fh) {
            Ok(p) => p,
            Err(e) => {
                if let Some(view) = &self.open_files {
                    // v1/v3 handles carry the original path; v4 kernel
                    // handles carry only the ino. Either identity finds
                    // the OPEN-anchored file (F17b).
                    let hit = FileHandleManager::parse_path_lenient(current_fh)
                        .ok()
                        .and_then(|p| view.file_for_path(&p).map(|f| (p, f)))
                        .or_else(|| {
                            FileHandleManager::object_ino(current_fh)
                                .and_then(|i| view.entry_for_ino(i))
                        });
                    {
                        if let Some((embedded, file)) = hit {
                            debug!("GETATTR: {:?} replaced on disk; fstat via open fd", embedded);
                            let snap = tokio::task::spawn_blocking(move || {
                                file.metadata()
                                    .and_then(|md| AttributeSnapshot::from_metadata(md, &embedded))
                            })
                            .await;
                            if let Ok(Ok(snapshot)) = snap {
                                let (attr_vals, supported_bitmap) =
                                    encode_attributes_from_snapshot(
                                        &op.attr_request,
                                        &snapshot,
                                        self.pnfs_enabled,
                                        // OPEN attr fast-path: layout attrs are
                                        // fsinfo-time reads answered per-volume by
                                        // handle_getattr; not derived here.
                                        None,
                                    );
                                return GetAttrRes {
                                    status: Nfs4Status::Ok,
                                    obj_attributes: Some(Fattr4 {
                                        attrmask: supported_bitmap,
                                        attr_vals,
                                    }),
                                };
                            }
                        }
                    }
                }
                warn!(
                    "GETATTR: Failed to resolve handle ({}): {}",
                    FileHandleManager::parse_path_lenient(current_fh)
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|_| "<unparseable>".into()),
                    e
                );
                return GetAttrRes {
                    status: Nfs4Status::Stale,
                    obj_attributes: None,
                };
            }
        };

        debug!("📂 GETATTR for path: {:?}", path);

        // PHASE 1: Fetch attribute snapshot (SINGLE VFS CALL)
        // This is the ONLY place where we do filesystem I/O for attributes
        let mut snapshot = match AttributeSnapshot::from_path(&path).await {
            Ok(s) => s,
            Err(e) => {
                warn!("GETATTR: Failed to create attribute snapshot for {:?}: {}", path, e);
                return GetAttrRes {
                    status: if e.kind() == std::io::ErrorKind::NotFound {
                        Nfs4Status::NoEnt
                    } else {
                        Nfs4Status::Io
                    },
                    obj_attributes: None,
                };
            }
        };
        
        // A striped file's bytes are on the DSes; the local stub has 0
        // blocks. Reporting that verbatim tells sparse-aware tools the
        // whole file is a hole — `tar --sparse` then backs up nothing.
        self.correct_space_used(&mut snapshot);

        // Debug log snapshot values (all from same point in time!)
        debug!("📊 Attribute snapshot for {:?}:", path);
        debug!("   type: {}, size: {}, fileid: {}", snapshot.ftype, snapshot.size, snapshot.fileid);
        debug!("   mode: {:o}, nlink: {}, owner: {}, group: {}", 
               snapshot.mode, snapshot.numlinks, snapshot.owner, snapshot.group);

        // PHASE 2: Encode from snapshot (NO VFS I/O, pure serialization)
        // Per RFC 8434 §13, all attributes MUST be from same point in time
        let (attr_vals, supported_bitmap) = encode_attributes_from_snapshot(
            &op.attr_request,
            &snapshot,
            self.pnfs_enabled,
            // Per-volume advertisement, riding the fsid: a mount of the
            // volume subtree crosses into the volume's own (synthetic)
            // filesystem, and THAT is the fsinfo probe that picks the
            // client's layout driver.
            self.scsi_fsid_for_path(&path),
        );
        
        let fattr = Fattr4 {
            attrmask: supported_bitmap.clone(),
            attr_vals: attr_vals.clone(),
        };

        debug!("GETATTR: Returning {} bytes of attributes (from snapshot)", fattr.attr_vals.len());
        
        // Detailed hex dump for debugging
        debug!("GETATTR: Supported bitmap: {:?}", supported_bitmap);
        if attr_vals.len() <= 256 {
            // Hex dump in 16-byte rows
            for (i, chunk) in attr_vals.chunks(16).enumerate() {
                let hex_str: String = chunk.iter().map(|b| format!("{:02x}", b)).collect::<Vec<_>>().join(" ");
                debug!("  Attr vals [{:04x}]: {}", i * 16, hex_str);
            }
        }

        GetAttrRes {
            status: Nfs4Status::Ok,
            obj_attributes: Some(fattr),
        }
    }

    /// Handle SETATTR operation
    pub async fn handle_setattr(
        &self,
        op: SetAttrOp,
        ctx: &CompoundContext,
    ) -> SetAttrRes {
        debug!("SETATTR");

        // Check current filehandle
        let current_fh = match &ctx.current_fh {
            Some(fh) => fh,
            None => {
                return SetAttrRes {
                    status: Nfs4Status::NoFileHandle,
                    attrsset: vec![],
                };
            }
        };

        // Resolve path
        let path = match self.fh_mgr.resolve_handle(current_fh) {
            Ok(p) => p,
            Err(e) => {
                warn!("SETATTR: Failed to resolve handle: {}", e);
                return SetAttrRes {
                    status: Nfs4Status::Stale,
                    attrsset: vec![],
                };
            }
        };

        // `apply_settable_attrs` re-checks existence with symlink_metadata()
        // (never follows links, so dangling symlinks still count as present).
        let decoded = match decode_settable_attrs(
            &op.obj_attributes.attrmask,
            &op.obj_attributes.attr_vals,
        ) {
            Ok(d) => d,
            Err(status) => {
                warn!("SETATTR: undecodable/unsupported attrs (mask {:?}) → {:?}",
                      op.obj_attributes.attrmask, status);
                return SetAttrRes { status, attrsset: vec![] };
            }
        };

        let (applied, err) = apply_settable_attrs_offloaded(path.clone(), decoded).await;
        debug!("SETATTR: applied attrs {:?} on {:?} (err={:?})", applied, path, err);
        SetAttrRes {
            // attrsset always reports what was actually set — including on
            // error, where it covers the attrs applied before the failure
            // (RFC 8881 §18.30.4).
            status: err.unwrap_or(Nfs4Status::Ok),
            attrsset: attr_numbers_to_bitmap(&applied),
        }
    }

    /// Handle READDIR operation
    /// Enumerate directory names starting immediately AFTER `cookie`,
    /// using the filesystem's OWN directory offsets as NFS cookies.
    ///
    /// This is the technique knfsd uses, and it is what makes a cookie
    /// mean "this entry" instead of "the Nth slot of whatever ordering
    /// this call happened to produce". `telldir` is taken *after* each
    /// entry, which is exactly NFSv4 cookie semantics: a later READDIR
    /// carrying that cookie resumes with the following entry.
    ///
    /// It replaces a design that re-enumerated and re-`stat`ed the ENTIRE
    /// directory on every call and then discarded everything before a
    /// positional index — O(entries) syscalls per call, so O(entries²)
    /// per full listing, and silently wrong the moment the directory
    /// changed underneath an outstanding cookie.
    ///
    /// `.` and `..` are skipped, preserving the previous behaviour
    /// (`tokio::fs::read_dir` never yielded them and NFSv4 clients
    /// synthesise them).
    ///
    /// Returns the batch plus whether the directory stream was exhausted.
    ///
    /// # Why this is Linux-only
    ///
    /// The technique requires `telldir` offsets to stay valid across
    /// *separate* `opendir` calls, which POSIX does not promise. Measured
    /// 2026-08-23: on Linux/ext4 a cookie taken after the 3rd entry
    /// (`2027179489069696846`) re-opens and seeks back to exactly the
    /// right entry; on macOS/APFS the same sequence yields cookie `0`
    /// and seeks back to `.`. Using it there would page forever over the
    /// same entries. Non-Linux therefore keeps the positional
    /// enumerator below, paired with a fine-grained `cookieverf` so a
    /// concurrent mutation is *detected* (NFS4ERR_NOT_SAME → the client
    /// restarts) rather than silently skipping entries. Both platforms
    /// are correct; only Linux is also O(1)-resume.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn read_dir_from_cookie(
        dir: &Path,
        cookie: u64,
        max_names: usize,
    ) -> std::io::Result<(Vec<(String, u64)>, bool)> {
        use std::ffi::{CStr, CString};
        use std::os::unix::ffi::OsStrExt;

        let c_path = CString::new(dir.as_os_str().as_bytes())
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has NUL"))?;

        // RAII so the stream is closed on EVERY exit path — early return,
        // `?`, or panic. `nix` would have given this for free, but its
        // `dir` module exposes only ino/name/file_type and no
        // telldir/seekdir, so it cannot express a stable cookie at all.
        struct DirStream(*mut libc::DIR);
        impl Drop for DirStream {
            fn drop(&mut self) {
                // SAFETY: constructed only from a non-null opendir result
                // and never closed elsewhere.
                unsafe { libc::closedir(self.0) };
            }
        }

        // SAFETY: `c_path` is a valid NUL-terminated path for the duration
        // of the call.
        let raw = unsafe { libc::opendir(c_path.as_ptr()) };
        if raw.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let stream = DirStream(raw);
        let dirp = stream.0;

        // `cookie == 0` means "from the beginning" (RFC 8881 §18.23.3),
        // which is where a freshly-opened stream already sits.
        if cookie != 0 {
            // SAFETY: dirp is a live DIR* from opendir above.
            unsafe { libc::seekdir(dirp, cookie as libc::c_long) };
        }

        let mut out = Vec::new();
        let mut eof = false;
        loop {
            if out.len() >= max_names {
                break;
            }
            // readdir(3) returns NULL for BOTH end-of-stream and error,
            // distinguishing them only by errno, so clear it first. The
            // accessor is libc-specific: glibc/musl expose
            // `__errno_location`, the BSDs (macOS) expose `__error`.
            // Getting this wrong would make a real I/O error read as a
            // clean end-of-directory — a truncated listing reported as
            // complete, which is the very class of bug being fixed here.
            #[cfg(any(target_os = "linux", target_os = "android"))]
            unsafe {
                *libc::__errno_location() = 0
            };
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            unsafe {
                *libc::__error() = 0
            };
            // SAFETY: dirp is live; the returned entry is owned by the
            // stream and is only read before the next readdir call.
            let ent = unsafe { libc::readdir(dirp) };
            if ent.is_null() {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error().unwrap_or(0) != 0 {
                    return Err(err);
                }
                eof = true;
                break;
            }
            // SAFETY: `ent` is non-null and points at a valid dirent.
            let name_bytes = unsafe { CStr::from_ptr((*ent).d_name.as_ptr()) }.to_bytes();
            // The cookie for THIS entry is the stream position after it.
            // SAFETY: dirp is live.
            let next = unsafe { libc::telldir(dirp) } as u64;

            if name_bytes == b"." || name_bytes == b".." {
                continue;
            }
            // Cookie 0 is reserved to mean "start of directory", so an
            // entry can never be handed out with it. This should not
            // occur (telldir after a successful readdir is past the
            // first entry), but handing it out would make the client
            // restart the listing forever.
            if next == 0 {
                continue;
            }
            out.push((String::from_utf8_lossy(name_bytes).into_owned(), next));
        }

        drop(stream);
        Ok((out, eof))
    }

    /// Non-Linux fallback: positional cookies over a full enumeration.
    ///
    /// Same signature as the Linux enumerator so the handler has exactly
    /// one code path. Cookies here are 1-based positions, which shift if
    /// the directory changes — so correctness depends on the
    /// `cookieverf` refusing a stale resume, and that verifier is
    /// nanosecond-granular for precisely this reason. Dev/test only;
    /// flint-lite ships on Linux.
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    fn read_dir_from_cookie(
        dir: &Path,
        cookie: u64,
        max_names: usize,
    ) -> std::io::Result<(Vec<(String, u64)>, bool)> {
        let mut all = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            all.push(entry.file_name().to_string_lossy().into_owned());
        }
        let start = cookie as usize;
        let mut out = Vec::new();
        for (idx, name) in all.iter().enumerate().skip(start) {
            if out.len() >= max_names {
                return Ok((out, false));
            }
            out.push((name.clone(), (idx + 1) as u64));
        }
        Ok((out, true))
    }

    pub async fn handle_readdir(
        &self,
        op: ReadDirOp,
        ctx: &CompoundContext,
    ) -> ReadDirRes {
        debug!("READDIR: cookie={}, maxcount={}", op.cookie, op.maxcount);

        // Check current filehandle
        let current_fh = match &ctx.current_fh {
            Some(fh) => fh,
            None => {
                return ReadDirRes {
                    status: Nfs4Status::NoFileHandle,
                    cookieverf: 0,
                    entries: vec![],
                    eof: true,
                };
            }
        };

        // Handle READDIR on pseudo-root - list exports
        if self.fh_mgr.is_pseudo_root(current_fh) {
            debug!("📂 READDIR on PSEUDO-ROOT");
            debug!("   cookie={}, cookieverf={:?}, dircount={}, maxcount={}", 
                   op.cookie, op.cookieverf, op.dircount, op.maxcount);
            
            let export_names = self.fh_mgr.get_pseudo_fs().list_exports();
            debug!("   Found {} exports: {:?}", export_names.len(), export_names);
            debug!("   Client requested {} attribute words: {:?}", op.attr_request.len(), op.attr_request);
            debug!("   Requested attribute bitmap: {:?}", op.attr_request);
            
            let mut entries = vec![];
            let mut total_size = 0usize;
            
            for (i, name) in export_names.iter().enumerate() {
                let entry_cookie = (i + 1) as u64;
                if op.cookie > 0 && entry_cookie <= op.cookie {
                    debug!("   Skipping entry '{}' (cookie {} <= requested {})", name, entry_cookie, op.cookie);
                    continue; // Skip entries before cookie
                }
                
                // Create attributes for export entry based on what client requested
                debug!("   Encoding entry '{}': client requested bitmap={:?}", name, op.attr_request);
                let (attr_vals, supported_bitmap) = encode_export_entry_attributes(name, &op.attr_request, self.pnfs_enabled);
                debug!("   → Returning for '{}': bitmap={:?}, {} bytes", name, supported_bitmap, attr_vals.len());
                
                // Decode bitmap to show which attributes (debug only)
                let mut attr_names = vec![];
                for (word_idx, &word) in supported_bitmap.iter().enumerate() {
                    for bit in 0..32 {
                        if (word & (1 << bit)) != 0 {
                            attr_names.push(word_idx * 32 + bit);
                        }
                    }
                }
                debug!("   → Attribute IDs: {:?}", attr_names);
                
                // Pre-encode Fattr4 into Bytes for compound module
                let mut fattr_buf = BytesMut::new();
                
                // Encode bitmap
                fattr_buf.put_u32(supported_bitmap.len() as u32);
                for word in &supported_bitmap {
                    fattr_buf.put_u32(*word);
                }
                
                // Encode attr_vals as opaque
                fattr_buf.put_u32(attr_vals.len() as u32);
                fattr_buf.put_slice(&attr_vals);
                let padding = (4 - (attr_vals.len() % 4)) % 4;
                for _ in 0..padding {
                    fattr_buf.put_u8(0);
                }
                
                let entry_size = fattr_buf.len();
                total_size += entry_size;
                debug!("   → Entry '{}' encoded: {} bytes (total so far: {})", name, entry_size, total_size);
                
                entries.push(CompoundDirEntry {
                    cookie: entry_cookie,
                    name: name.clone(),
                    attrs: fattr_buf.freeze(),
                });
                
                // Check if we've exceeded maxcount
                if total_size > op.maxcount as usize {
                    debug!("   Stopping: total_size {} exceeds maxcount {}", total_size, op.maxcount);
                    break;
                }
            }
            
            debug!("✅ READDIR returning {} export entries (no . or .. per NFSv4 spec), total {} bytes", 
                  entries.len(), total_size);
            debug!("   Entry names: {:?}", entries.iter().map(|e| &e.name).collect::<Vec<_>>());
            
            return ReadDirRes {
                status: Nfs4Status::Ok,
                cookieverf: 1, // Simple verifier for pseudo-root (exports don't change)
                entries,
                eof: true,
            };
        }

        // Handle READDIR on regular directories
        // Resolve the directory path from the filehandle
        let dir_path = match self.fh_mgr.resolve_handle(current_fh) {
            Ok(p) => p,
            Err(e) => {
                warn!("READDIR: Failed to resolve handle: {}", e);
                return ReadDirRes {
                    status: Nfs4Status::Stale,
                    cookieverf: 0,
                    entries: vec![],
                    eof: true,
                };
            }
        };

        debug!("READDIR: Reading directory: {:?}", dir_path);

        // Get directory metadata for cookieverf generation
        // Per RFC 5661 Section 18.23.3, cookieverf is used to detect directory changes
        let dir_metadata = match tokio::fs::metadata(&dir_path).await {
            Ok(m) => m,
            Err(e) => {
                warn!("READDIR: Failed to stat directory: {}", e);
                let status = if e.kind() == std::io::ErrorKind::NotFound {
                    Nfs4Status::NoEnt
                } else if e.kind() == std::io::ErrorKind::PermissionDenied {
                    Nfs4Status::Access
                } else {
                    Nfs4Status::Io
                };
                return ReadDirRes {
                    status,
                    cookieverf: 0,
                    entries: vec![],
                    eof: true,
                };
            }
        };

        // Generate cookieverf from directory mtime
        // This allows clients to detect if directory changed between READDIR calls
        let current_cookieverf = match dir_metadata.modified() {
            Ok(mtime) => {
                // NANOSECONDS, not seconds. At second granularity any
                // mutation landing in the same wall-clock second as the
                // previous READDIR left the verifier unchanged, so the
                // server confirmed "directory unchanged" while positional
                // cookies had already shifted — and a file that existed
                // for the whole listing was silently omitted from an
                // NFS4_OK reply (pinned by
                // `readdir_does_not_skip_entries_when_dir_changes_in_one_second`).
                // Sub-second churn is the normal case for the fleet
                // workload, not an edge case.
                mtime.duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or(std::time::Duration::from_secs(0))
                    .as_nanos() as u64
            }
            Err(_) => {
                // Fallback if mtime not available (shouldn't happen on Unix)
                1u64
            }
        };

        // Validate cookieverf on subsequent requests (when cookie != 0)
        // Per RFC 5661: "If the server determines that the cookieverf is no longer valid
        // for the directory, the error NFS4ERR_NOT_SAME must be returned."
        if op.cookie != 0 && op.cookieverf != 0 {
            if op.cookieverf != current_cookieverf {
                debug!("READDIR: cookieverf mismatch - directory changed (expected {}, got {})",
                       current_cookieverf, op.cookieverf);
                return ReadDirRes {
                    status: Nfs4Status::NotSame,
                    cookieverf: current_cookieverf,
                    entries: vec![],
                    eof: true,
                };
            }
            debug!("READDIR: cookieverf validated successfully");
        }

        // Enumerate from the client's cookie using the filesystem's own
        // directory offsets. Only the entries we may actually return get
        // fetched, and only those get `stat`ed — the old path walked and
        // stat'ed the whole directory on every single call.
        //
        // The batch cap bounds one call's work; `maxcount`/`dircount`
        // below usually stop us well before it. A batch that ends without
        // `eof` simply means the next READDIR resumes at the last cookie.
        const READDIR_BATCH: usize = 1024;
        let scan_dir = dir_path.clone();
        let scan_cookie = op.cookie;
        let scanned = tokio::task::spawn_blocking(move || {
            Self::read_dir_from_cookie(&scan_dir, scan_cookie, READDIR_BATCH)
        })
        .await;

        let (names, stream_eof) = match scanned {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                warn!("READDIR: Failed to read directory: {}", e);
                let status = if e.kind() == std::io::ErrorKind::NotFound {
                    Nfs4Status::NoEnt
                } else if e.kind() == std::io::ErrorKind::PermissionDenied {
                    Nfs4Status::Access
                } else {
                    Nfs4Status::Io
                };
                return ReadDirRes { status, cookieverf: 0, entries: vec![], eof: true };
            }
            Err(e) => {
                warn!("READDIR: directory scan task failed: {}", e);
                return ReadDirRes {
                    status: Nfs4Status::Io,
                    cookieverf: 0,
                    entries: vec![],
                    eof: true,
                };
            }
        };

        debug!("READDIR: scanned {} entries from cookie {} (stream_eof={})",
               names.len(), op.cookie, stream_eof);

        // Stat only what we scanned. An entry that vanishes between the
        // scan and the stat is dropped from THIS reply, exactly as before
        // — but it no longer shifts anyone else's cookie, because cookies
        // come from the directory stream rather than from this vector's
        // indices.
        let mut all_entries = Vec::with_capacity(names.len());
        for (file_name, entry_cookie) in names {
            let entry_path = dir_path.join(&file_name);
            let mut snapshot = match AttributeSnapshot::from_path(&entry_path).await {
                Ok(s) => s,
                Err(e) => {
                    debug!("READDIR: Failed to stat '{}': {}, skipping", file_name, e);
                    continue;
                }
            };
            // Same correction as GETATTR: a READDIR carrying attributes
            // must not report a striped file as fully unallocated either.
            self.correct_space_used(&mut snapshot);
            all_entries.push((file_name, entry_cookie, snapshot));
        }

        // Build response entries with attribute encoding
        let mut response_entries = Vec::new();
        let mut total_bytes = 0usize;
        let mut dir_bytes_used = 0usize;

        // Base sizes per RFC 5661
        // READDIR response includes: status(4) + cookieverf(8) + entry_follows(4) + eof(4)
        const BASE_RESPONSE_SIZE: usize = 20;
        let maxcount_limit = op.maxcount.saturating_sub(BASE_RESPONSE_SIZE as u32) as usize;

        for (file_name, entry_cookie, snapshot) in all_entries.iter() {
            // The cookie is the directory stream's own offset past this
            // entry — stable across calls and across mutation elsewhere
            // in the directory.
            let cookie = *entry_cookie;

            // Encode attributes based on client's request
            let (attr_vals, supported_bitmap) = encode_attributes_from_snapshot(
                &op.attr_request,
                snapshot,
                self.pnfs_enabled,
                // Per ENTRY, not per directory: a listing of the export
                // root enumerates the volume dirs themselves, and a
                // scsi volume's entry must already carry its own fsid
                // or READDIRPLUS would report it as part of the parent
                // filesystem it is not in.
                self.scsi_fsid_for_path(&dir_path.join(file_name)),
            );

            debug!("READDIR: Encoding '{}': {} attribute bytes, bitmap={:?}",
                   file_name, attr_vals.len(), supported_bitmap);
            debug!("   → File snapshot: type={}, mode={:o}, nlink={}, owner={}, group={}, size={}",
                   snapshot.ftype, snapshot.mode, snapshot.numlinks, snapshot.owner, snapshot.group, snapshot.size);
            debug!("   → FSID=({}, {}), fileid={}", snapshot.fsid_major, snapshot.fsid_minor, snapshot.fileid);

            // Build fattr4 structure per RFC 5661
            // fattr4 = attrmask (array of u32) + attr_vals (opaque bytes with length prefix)
            let mut fattr_buf = BytesMut::new();

            // Encode attrmask (bitmap)
            fattr_buf.put_u32(supported_bitmap.len() as u32);
            for word in &supported_bitmap {
                fattr_buf.put_u32(*word);
            }

            // Encode attr_vals as opaque (length + data + padding)
            fattr_buf.put_u32(attr_vals.len() as u32);
            fattr_buf.put_slice(&attr_vals);
            let padding = (4 - (attr_vals.len() % 4)) % 4;
            for _ in 0..padding {
                fattr_buf.put_u8(0);
            }

            let fattr_bytes = fattr_buf.freeze();

            // Calculate size of this entry4 on the wire
            // entry4 = cookie(8) + name_len(4) + name(variable) + name_padding + attrs + next_entry_flag(4)
            let name_len_padded = ((file_name.len() + 3) / 4) * 4; // Round up to 4-byte boundary
            let entry_wire_size = 8 + 4 + name_len_padded + fattr_bytes.len() + 4;

            // Check maxcount limit (total response size)
            if total_bytes + entry_wire_size > maxcount_limit {
                debug!("READDIR: Hit maxcount limit at {} entries", response_entries.len());
                break;
            }

            // Calculate dircount contribution (cookie + name only per RFC)
            let dir_bytes_contribution = 8 + 4 + name_len_padded;

            // Check dircount limit (directory info only, no attributes)
            if op.dircount > 0 && dir_bytes_used + dir_bytes_contribution > op.dircount as usize {
                debug!("READDIR: Hit dircount limit at {} entries", response_entries.len());
                break;
            }

            // Add this entry to response
            response_entries.push(CompoundDirEntry {
                cookie,
                name: file_name.clone(),
                attrs: fattr_bytes,
            });

            total_bytes += entry_wire_size;
            dir_bytes_used += dir_bytes_contribution;
        }

        // EOF only when the directory stream itself was exhausted AND we
        // encoded everything we scanned. If a size limit cut the batch
        // short, the client resumes from the last entry's cookie.
        let eof = stream_eof && response_entries.len() == all_entries.len();

        debug!("READDIR: Returning {} entries, eof={}, total_bytes={}",
               response_entries.len(), eof, total_bytes);

        ReadDirRes {
            status: Nfs4Status::Ok,
            cookieverf: current_cookieverf, // Directory mtime as verifier per RFC 5661
            entries: response_entries,
            eof,
        }
    }

    /// Handle CREATE operation
    pub async fn handle_create(
        &self,
        op: CreateOp,
        ctx: &mut CompoundContext,
    ) -> CreateRes {
        debug!("CREATE: type={:?}, name={}", op.objtype, op.objname);

        if let Some(status) = validate_component_name(&op.objname) {
            warn!("CREATE: invalid object name → {:?}", status);
            return CreateRes { status, change_info: None, attrset: vec![] };
        }

        // Decode createattrs BEFORE creating anything: a malformed or
        // unsupported attr fails the op with no side effects (RFC 8881
        // §18.4.3). Ignoring these was another fake-OK — initdb's
        // mkdir(0700) came out 0755 and postgres refused the directory.
        let want_attrs = match decode_settable_attrs(
            &op.createattrs.attrmask,
            &op.createattrs.attr_vals,
        ) {
            Ok(want) => want,
            Err(status) => {
                warn!("CREATE: bad createattrs → {:?}", status);
                return CreateRes { status, change_info: None, attrset: vec![] };
            }
        };

        // Check current filehandle (parent directory)
        let parent_fh = match &ctx.current_fh {
            Some(fh) => fh,
            None => {
                return CreateRes {
                    status: Nfs4Status::NoFileHandle,
                    change_info: None,
                    attrset: vec![],
                };
            }
        };

        // Resolve parent directory path
        let parent_path = match self.fh_mgr.resolve_handle(parent_fh) {
            Ok(p) => p,
            Err(e) => {
                warn!("CREATE: Failed to resolve parent handle: {}", e);
                return CreateRes {
                    status: Nfs4Status::Stale,
                    change_info: None,
                    attrset: vec![],
                };
            }
        };

        // Build full path for the new object. For symlinks, RFC 5661 §18.6:
        //   objname  = the new link's component name in `parent_path`
        //   linkdata = what the link points to (target text)
        // The previous code swapped them, but only because the CREATE
        // decoder had its field order wrong (objname read before the union
        // tail for NF4LNK). With the decoder corrected, the ops carry the
        // RFC-correct semantics directly.
        let (obj_path, symlink_target) = if op.objtype == Nfs4FileType::Symlink {
            if let Some(target) = &op.linkdata {
                (parent_path.join(&op.objname), Some(target.clone()))
            } else {
                warn!("CREATE: Symlink without linkdata");
                return CreateRes {
                    status: Nfs4Status::Inval,
                    change_info: None,
                    attrset: vec![],
                };
            }
        } else {
            (parent_path.join(&op.objname), None)
        };

        // A10 admission: refuse new-object creation with NOSPC past
        // the reserve (one relaxed load when the tier is off).
        if crate::tier::space::admit_create(&obj_path).is_err() {
            warn!("CREATE: refused NOSPC — PVC headroom-minus-reserve exhausted");
            return CreateRes { status: Nfs4Status::NoSpc, change_info: None, attrset: vec![] };
        }

        // Create the object based on type
        let create_result = match op.objtype {
            Nfs4FileType::Regular => {
                // Create regular file
                // Through open_beneath: File::create means
                // O_WRONLY|O_CREAT|O_TRUNC, so CREATE(NF4LNK, "x" ->
                // /data/state/state.db) followed by CREATE(NF4REG,
                // "x") used to truncate whatever the link named.
                crate::nfs::v4::open_beneath::open_async(
                    tokio::fs::OpenOptions::new().write(true).create(true).truncate(true),
                    &obj_path,
                )
                .await
                .map(|_| ())
            }
            Nfs4FileType::Directory => {
                // Create directory
                tokio::fs::create_dir(&obj_path).await
            }
            Nfs4FileType::Symlink => {
                // Create symlink
                if let Some(target) = symlink_target {
                    debug!("CREATE: Creating symlink at {:?} pointing to '{}'", obj_path, target);
                    #[cfg(unix)]
                    {
                        tokio::fs::symlink(&target, &obj_path).await
                    }
                    #[cfg(not(unix))]
                    {
                        return CreateRes {
                            status: Nfs4Status::NotSupp,
                            change_info: None,
                            attrset: vec![],
                        };
                    }
                } else {
                    // Should never happen
                    return CreateRes {
                        status: Nfs4Status::Inval,
                        change_info: None,
                        attrset: vec![],
                    };
                }
            }
            // Special-file types (SOCK, FIFO, BLK, CHR). We don't currently
            // model them as native nodes — creating BLK/CHR via mknod() needs
            // root, and pNFS-CSI exports never want callers actually
            // dereferencing a host device. Create a regular file as a
            // stand-in so LOOKUP/PUTFH/GETFH/REMOVE on the name work; tests
            // that depend on the *type* (NF4SOCK etc.) will still fail.
            // Returning Ok also keeps pynfs's --maketree from skipping
            // every test that names the file later.
            Nfs4FileType::Socket
            | Nfs4FileType::Fifo
            | Nfs4FileType::BlockDevice
            | Nfs4FileType::CharDevice => {
                debug!("CREATE: {:?} → regular-file stand-in at {:?}",
                       op.objtype, obj_path);
                // Through open_beneath: File::create means
                // O_WRONLY|O_CREAT|O_TRUNC, so CREATE(NF4LNK, "x" ->
                // /data/state/state.db) followed by CREATE(NF4REG,
                // "x") used to truncate whatever the link named.
                crate::nfs::v4::open_beneath::open_async(
                    tokio::fs::OpenOptions::new().write(true).create(true).truncate(true),
                    &obj_path,
                )
                .await
                .map(|_| ())
            }
            _ => {
                warn!("CREATE: Object type {:?} not yet supported", op.objtype);
                return CreateRes {
                    status: Nfs4Status::BadType,
                    change_info: None,
                    attrset: vec![],
                };
            }
        };

        match create_result {
            Ok(_) => {
                // F14: new object + new parent dirent.
                crate::nfs::v4::change_counter::bump_path(&obj_path);
                if let Some(parent) = obj_path.parent() {
                    crate::nfs::v4::change_counter::bump_path(parent);
                }

                // A newly created REGULAR file (including the special-file
                // stand-ins, which are regular files on disk) needs a
                // capture note or it never becomes a bucket object: no
                // note means no dirty row, no generation row, and nothing
                // for the manifest to record as restorable, so the file
                // exists locally and is absent from every restore. Dirs
                // and symlinks are exempt — they carry no S3 object and
                // the manifest reconstructs them from the tree walk.
                // Inside a write ticket, per gate.rs's straggler
                // invariant; see the matching note in OPEN(create).
                let creates_regular_file = matches!(
                    op.objtype,
                    Nfs4FileType::Regular
                        | Nfs4FileType::Socket
                        | Nfs4FileType::Fifo
                        | Nfs4FileType::BlockDevice
                        | Nfs4FileType::CharDevice
                );
                if creates_regular_file {
                    if let Err(crate::tier::gate::Excluded) =
                        crate::tier::gate::enter_path(&obj_path).map(|_ticket| {
                            crate::tier::capture::note_path(
                                &obj_path,
                                crate::tier::capture::Mutation::Whole,
                            )
                        })
                    {
                        warn!(
                            "CREATE: write gate excluded a fresh file at {:?} — noting \
                             dirty outside the ticket",
                            obj_path
                        );
                        crate::tier::capture::note_path(
                            &obj_path,
                            crate::tier::capture::Mutation::Whole,
                        );
                    }
                }

                // Stamp the caller's AUTH_SYS identity on the new object
                // (dirs/symlinks/stand-ins) — client-side permission checks
                // compare mode bits against st_uid, so a root-owned 0700
                // directory would lock its creator out. Best effort.
                if let Some((uid, gid)) = ctx.unix_cred {
                    let p = obj_path.clone();
                    let is_symlink = op.objtype == Nfs4FileType::Symlink;
                    let _ = tokio::task::spawn_blocking(move || {
                        if is_symlink {
                            std::os::unix::fs::lchown(&p, Some(uid), Some(gid))
                        } else {
                            std::os::unix::fs::chown(&p, Some(uid), Some(gid))
                        }
                    })
                    .await;
                }

                // Apply the requested createattrs (mode, explicit owner —
                // which then wins over the cred stamp above, times). The
                // object exists at this point, so an application failure is
                // logged and reflected in attrset rather than unwinding the
                // create; symlink modes are meaningless on Linux and skipped.
                let applied = if op.objtype == Nfs4FileType::Symlink {
                    Vec::new()
                } else {
                    let (applied, apply_err) =
                        apply_settable_attrs_offloaded(obj_path.clone(), want_attrs.clone()).await;
                    if let Some(e) = apply_err {
                        warn!("CREATE: createattrs on {:?} partially applied: {:?}", obj_path, e);
                    }
                    applied
                };

                // Generate filehandle for new object
                match self.fh_mgr.get_or_create_handle(&obj_path) {
                    Ok(new_fh) => {
                        // Set new filehandle as current
                        ctx.current_fh = Some(new_fh);

                        CreateRes {
                            status: Nfs4Status::Ok,
                            change_info: Some(ChangeInfo {
                                atomic: true,
                                before: 0,
                                after: 1,
                            }),
                            attrset: attr_numbers_to_bitmap(&applied),
                        }
                    }
                    Err(e) => {
                        warn!("CREATE: Failed to generate handle: {}", e);
                        CreateRes {
                            status: Nfs4Status::Io,
                            change_info: None,
                            attrset: vec![],
                        }
                    }
                }
            }
            Err(e) => {
                warn!("CREATE: Failed to create {}: {}", op.objname, e);
                let status = match e.kind() {
                    std::io::ErrorKind::AlreadyExists => Nfs4Status::Exist,
                    std::io::ErrorKind::PermissionDenied => Nfs4Status::Access,
                    std::io::ErrorKind::NotFound => Nfs4Status::NoEnt,
                    _ => Nfs4Status::Io,
                };
                CreateRes {
                    status,
                    change_info: None,
                    attrset: vec![],
                }
            }
        }
    }

    /// Handle REMOVE operation
    pub async fn handle_remove(
        &self,
        op: RemoveOp,
        ctx: &CompoundContext,
    ) -> RemoveRes {
        debug!("REMOVE: target={}", op.target);

        if let Some(status) = validate_component_name(&op.target) {
            warn!("REMOVE: invalid target name → {:?}", status);
            return RemoveRes { status, change_info: None };
        }

        // Check current filehandle (parent directory)
        let parent_fh = match &ctx.current_fh {
            Some(fh) => fh,
            None => {
                return RemoveRes {
                    status: Nfs4Status::NoFileHandle,
                    change_info: None,
                };
            }
        };

        // Resolve parent directory path
        let parent_path = match self.fh_mgr.resolve_handle(parent_fh) {
            Ok(p) => p,
            Err(e) => {
                warn!("REMOVE: Failed to resolve parent handle: {}", e);
                return RemoveRes {
                    status: Nfs4Status::Stale,
                    change_info: None,
                };
            }
        };

        // Build full path for target
        let target_path = parent_path.join(&op.target);

        // Check if target is a directory or file. symlink_metadata: a
        // dangling symlink is still a removable directory entry —
        // metadata() would follow the link, report NotFound, and make the
        // entry undeletable over NFS.
        match tokio::fs::symlink_metadata(&target_path).await {
            Ok(metadata) => {
                // A7: the victim's identity, resolved BEFORE the
                // unlink (unrecoverable after). Regular files only —
                // directories and symlinks carry no tier rows.
                #[cfg(unix)]
                let tier_ident = (crate::tier::capture::enabled() && metadata.is_file())
                    .then(|| {
                        use std::os::unix::fs::MetadataExt;
                        (metadata.dev(), metadata.ino())
                    });
                let result = if metadata.is_dir() {
                    tokio::fs::remove_dir(&target_path).await
                } else {
                    tokio::fs::remove_file(&target_path).await
                };

                match result {
                    Ok(_) => {
                        self.fh_mgr.note_fs_remove(&target_path);
                        // F14: removed dirent mutates the parent.
                        crate::nfs::v4::change_counter::bump_path(&parent_path);
                        // A7: tombstone the removed file's generation
                        // (durable pre-ack via the dispatcher drain).
                        #[cfg(unix)]
                        if let Some(id) = tier_ident {
                            crate::tier::identity::note_remove(id);
                        }
                        RemoveRes {
                            status: Nfs4Status::Ok,
                            change_info: Some(ChangeInfo {
                                atomic: true,
                                before: 1,
                                after: 2,
                            }),
                        }
                    }
                    Err(e) => {
                        warn!("REMOVE: Failed to remove {}: {}", op.target, e);
                        let status = match e.kind() {
                            std::io::ErrorKind::PermissionDenied => Nfs4Status::Access,
                            std::io::ErrorKind::NotFound => Nfs4Status::NoEnt,
                            std::io::ErrorKind::DirectoryNotEmpty => Nfs4Status::NotEmpty,
                            _ => Nfs4Status::Io,
                        };
                        RemoveRes {
                            status,
                            change_info: None,
                        }
                    }
                }
            }
            Err(e) => {
                warn!("REMOVE: Failed to stat {}: {}", op.target, e);
                RemoveRes {
                    status: Nfs4Status::NoEnt,
                    change_info: None,
                }
            }
        }
    }

    /// Handle RENAME operation (RFC 5661 §18.26).
    ///
    /// Source = saved_fh / oldname, target = current_fh / newname.
    /// Validation order matches the RFC's listed errors so pynfs's negative
    /// tests get the codes they expect:
    ///   1. NoFileHandle if either FH is unset.
    ///   2. NotDir if either parent FH does not resolve to a directory.
    ///   3. Inval for empty oldname/newname (component4 cannot be empty).
    ///   4. BadName for "." or "..".
    ///   5. NoEnt if the source object does not exist.
    ///   6. Operating-system specific errors mapped to NFS4ERR_*.
    /// On success, source_cinfo / target_cinfo report cinfo for the parent
    /// directories. RFC 5661 §18.26.4 also says self-rename ("rename foo to
    /// foo in the same directory") MUST report unchanged cinfo for the
    /// directory; we detect that case and replay the same before/after pair.
    pub async fn handle_rename(
        &self,
        op: RenameOp,
        ctx: &CompoundContext,
    ) -> RenameRes {
        debug!("RENAME: {} -> {}", op.oldname, op.newname);

        if let Some(status) =
            validate_component_name(&op.oldname).or_else(|| validate_component_name(&op.newname))
        {
            warn!("RENAME: invalid component name → {:?}", status);
            return rename_err(status);
        }

        // (1) Both filehandles must be set.
        let source_parent_fh = match &ctx.saved_fh {
            Some(fh) => fh,
            None => return rename_err(Nfs4Status::NoFileHandle),
        };
        let dest_parent_fh = match &ctx.current_fh {
            Some(fh) => fh,
            None => return rename_err(Nfs4Status::NoFileHandle),
        };

        // (3) component4 cannot be empty.
        if op.oldname.is_empty() || op.newname.is_empty() {
            warn!("RENAME: empty component (old='{}', new='{}')", op.oldname, op.newname);
            return rename_err(Nfs4Status::Inval);
        }
        // (4) "." and ".." are reserved per RFC 5661 §1.7 / §18.26.3.
        if op.oldname == "." || op.oldname == ".."
            || op.newname == "." || op.newname == ".."
        {
            return rename_err(Nfs4Status::BadName);
        }

        // Resolve parent directory paths.
        let source_parent_path = match self.fh_mgr.resolve_handle(source_parent_fh) {
            Ok(p) => p,
            Err(_) => return rename_err(Nfs4Status::Stale),
        };
        let dest_parent_path = match self.fh_mgr.resolve_handle(dest_parent_fh) {
            Ok(p) => p,
            Err(_) => return rename_err(Nfs4Status::Stale),
        };

        // (2) Both must be directories.
        let source_parent_meta = match source_parent_path.symlink_metadata() {
            Ok(m) => m,
            Err(_) => return rename_err(Nfs4Status::Stale),
        };
        if !source_parent_meta.is_dir() {
            return rename_err(Nfs4Status::NotDir);
        }
        let dest_parent_meta = match dest_parent_path.symlink_metadata() {
            Ok(m) => m,
            Err(_) => return rename_err(Nfs4Status::Stale),
        };
        if !dest_parent_meta.is_dir() {
            return rename_err(Nfs4Status::NotDir);
        }

        let source_path = source_parent_path.join(&op.oldname);
        let dest_path = dest_parent_path.join(&op.newname);

        // (5) Source must exist (use symlink_metadata so a dangling symlink
        // is still considered present — the link itself is the object we'd
        // be renaming).
        if source_path.symlink_metadata().is_err() {
            return rename_err(Nfs4Status::NoEnt);
        }

        // RFC 5661 §18.26.4: rename to the same name in the same directory
        // is a no-op and the cinfo MUST report no change. Detect by canonical
        // parent paths + identical name.
        let mut is_self_rename = op.oldname == op.newname
            && source_parent_path == dest_parent_path;

        // POSIX rename(2) extends this: when source and destination are
        // hard links to the SAME inode, rename does nothing and reports
        // success — so the cinfo must also show no change (pynfs RNM20).
        #[cfg(unix)]
        if !is_self_rename {
            use std::os::unix::fs::MetadataExt;
            if let (Ok(s), Ok(d)) = (source_path.symlink_metadata(), dest_path.symlink_metadata()) {
                if s.dev() == d.dev() && s.ino() == d.ino() && !s.file_type().is_symlink() {
                    is_self_rename = true;
                }
            }
        }

        // Pre-check destination: if it exists, semantics depend on the types
        // of source and dest (RFC 5661 §18.26.4):
        //   - both regular files → atomic replace, OK.
        //   - source dir, dest non-dir → NFS4ERR_NOTDIR.
        //   - source non-dir, dest dir → NFS4ERR_ISDIR.
        //   - source dir, dest dir non-empty → NFS4ERR_NOTEMPTY (or EXIST).
        //   - source dir, dest dir empty → atomic replace, OK.
        // Emulate the typed errors here; the underlying tokio::fs::rename
        // would otherwise just return ErrorKind::Other on these.
        // A7: the covered destination's identity, resolved BEFORE the
        // rename destroys its last name — rename-over must tombstone
        // its generation atomically with the rename's durable half.
        #[cfg(unix)]
        let mut tier_covered: Option<(u64, u64)> = None;
        if !is_self_rename {
            if let Ok(dest_meta) = dest_path.symlink_metadata() {
                #[cfg(unix)]
                if crate::tier::capture::enabled() && dest_meta.is_file() {
                    use std::os::unix::fs::MetadataExt;
                    tier_covered = Some((dest_meta.dev(), dest_meta.ino()));
                }
                let src_is_dir = source_path
                    .symlink_metadata()
                    .map(|m| m.is_dir())
                    .unwrap_or(false);
                let dest_is_dir = dest_meta.is_dir();
                match (src_is_dir, dest_is_dir) {
                    (true, false) => return rename_err(Nfs4Status::Exist),
                    (false, true) => return rename_err(Nfs4Status::Exist),
                    (true, true) => {
                        // dest is a non-empty dir → NotEmpty (a read error
                        // also refuses: we can't prove the dir is empty)
                        if let Ok(mut entries) = tokio::fs::read_dir(&dest_path).await {
                            if !matches!(entries.next_entry().await, Ok(None)) {
                                return rename_err(Nfs4Status::NotEmpty);
                            }
                        }
                    }
                    (false, false) => { /* atomic replace allowed */ }
                }
            }
        }

        // Perform the rename.
        match tokio::fs::rename(&source_path, &dest_path).await {
            Ok(_) => {
                // Keep the filehandle tables truthful: v2 (id-based)
                // handles follow the file; stale v1 cache entries for
                // the old subtree are dropped.
                self.fh_mgr.note_fs_rename(&source_path, &dest_path);
                // F14: both parents' dirents changed, and the moved
                // object's own ctime bumped (rename updates it).
                if let Some(p) = source_path.parent() {
                    crate::nfs::v4::change_counter::bump_path(p);
                }
                if let Some(p) = dest_path.parent() {
                    crate::nfs::v4::change_counter::bump_path(p);
                }
                crate::nfs::v4::change_counter::bump_path(&dest_path);
                // A7: queue the identity event — the moved file's bit
                // re-points at the new path (the flusher re-keys), the
                // covered file's generation tombstones atomically.
                // Durable pre-ack via the dispatcher drain. Regular
                // files only; self-renames change nothing.
                #[cfg(unix)]
                if crate::tier::capture::enabled() && !is_self_rename {
                    use std::os::unix::fs::MetadataExt;
                    let moved = dest_path
                        .symlink_metadata()
                        .ok()
                        .filter(|m| m.is_file())
                        .map(|m| (m.dev(), m.ino()));
                    if moved.is_some() || tier_covered.is_some() {
                        crate::tier::identity::note_rename(moved, &dest_path, tier_covered);
                    }
                }
                let cinfo = if is_self_rename {
                    // No actual change to the directory.
                    ChangeInfo { atomic: true, before: 1, after: 1 }
                } else {
                    ChangeInfo { atomic: true, before: 1, after: 2 }
                };
                RenameRes {
                    status: Nfs4Status::Ok,
                    source_cinfo: Some(cinfo.clone()),
                    target_cinfo: Some(cinfo),
                }
            }
            Err(e) => {
                warn!("RENAME: Failed to rename {:?} to {:?}: {}", source_path, dest_path, e);
                let status = match e.kind() {
                    std::io::ErrorKind::NotFound => Nfs4Status::NoEnt,
                    std::io::ErrorKind::PermissionDenied => Nfs4Status::Access,
                    std::io::ErrorKind::AlreadyExists => Nfs4Status::Exist,
                    _ => Nfs4Status::Io,
                };
                rename_err(status)
            }
        }
    }

    /// Handle LINK operation (RFC 7862 Section 15.4)
    ///
    /// Creates a hard link to current FH in saved FH directory.
    /// Requires: current_fh (existing file), saved_fh (target directory)
    pub async fn handle_link(
        &self,
        op: LinkOp,
        ctx: &CompoundContext,
    ) -> LinkRes {
        debug!("LINK: new name={}", op.newname);

        if let Some(status) = validate_component_name(&op.newname) {
            warn!("LINK: invalid new name → {:?}", status);
            return LinkRes { status, change_info: None };
        }

        // RFC 8881 §18.9.3: the file to link is the SAVED filehandle; the
        // target directory is the CURRENT filehandle (clients send
        // PUTFH(file) SAVEFH PUTFH(dir) LINK). These were swapped, so every
        // LINK tried to hard-link the directory → EPERM → NFS4ERR_ACCESS.
        let file_fh = match &ctx.saved_fh {
            Some(fh) => fh,
            None => {
                return LinkRes {
                    status: Nfs4Status::NoFileHandle,
                    change_info: None,
                };
            }
        };

        let target_dir_fh = match &ctx.current_fh {
            Some(fh) => fh,
            None => {
                return LinkRes {
                    status: Nfs4Status::NoFileHandle,
                    change_info: None,
                };
            }
        };

        // Resolve existing file path
        let file_path = match self.fh_mgr.resolve_handle(file_fh) {
            Ok(p) => p,
            Err(e) => {
                warn!("LINK: Failed to resolve file handle: {}", e);
                return LinkRes {
                    status: Nfs4Status::Stale,
                    change_info: None,
                };
            }
        };

        // Resolve target directory path
        let target_dir_path = match self.fh_mgr.resolve_handle(target_dir_fh) {
            Ok(p) => p,
            Err(e) => {
                warn!("LINK: Failed to resolve target dir handle: {}", e);
                return LinkRes {
                    status: Nfs4Status::Stale,
                    change_info: None,
                };
            }
        };

        // Build path for new link
        let link_path = target_dir_path.join(&op.newname);

        // Create hard link
        match tokio::fs::hard_link(&file_path, &link_path).await {
            Ok(_) => {
                debug!("LINK: Successfully created hard link {:?} -> {:?}", link_path, file_path);
                // F14: the linked file's nlink/ctime changed and the
                // target parent gained a dirent.
                crate::nfs::v4::change_counter::bump_path(&file_path);
                if let Some(p) = link_path.parent() {
                    crate::nfs::v4::change_counter::bump_path(p);
                }
                LinkRes {
                    status: Nfs4Status::Ok,
                    change_info: Some(ChangeInfo {
                        atomic: true,
                        before: 1,
                        after: 2,
                    }),
                }
            }
            Err(e) => {
                warn!("LINK: Failed to create hard link {:?} -> {:?}: {}", link_path, file_path, e);
                let status = match e.kind() {
                    std::io::ErrorKind::NotFound => Nfs4Status::NoEnt,
                    std::io::ErrorKind::PermissionDenied => Nfs4Status::Access,
                    std::io::ErrorKind::AlreadyExists => Nfs4Status::Exist,
                    std::io::ErrorKind::InvalidInput => Nfs4Status::NotDir, // Source is directory
                    _ => Nfs4Status::Io,
                };
                LinkRes {
                    status,
                    change_info: None,
                }
            }
        }
    }

    /// Handle READLINK operation (RFC 7862 Section 15.8)
    ///
    /// Reads the target of a symbolic link.
    pub async fn handle_readlink(
        &self,
        _op: ReadLinkOp,
        ctx: &CompoundContext,
    ) -> ReadLinkRes {
        debug!("READLINK");

        // Check current filehandle
        let link_fh = match &ctx.current_fh {
            Some(fh) => fh,
            None => {
                return ReadLinkRes {
                    status: Nfs4Status::NoFileHandle,
                    link: None,
                };
            }
        };

        // Resolve symlink path
        let link_path = match self.fh_mgr.resolve_handle(link_fh) {
            Ok(p) => p,
            Err(e) => {
                warn!("READLINK: Failed to resolve handle: {}", e);
                return ReadLinkRes {
                    status: Nfs4Status::Stale,
                    link: None,
                };
            }
        };

        // Read the symbolic link
        match tokio::fs::read_link(&link_path).await {
            Ok(target) => {
                let target_str = target.to_string_lossy().to_string();
                debug!("READLINK: {:?} -> {}", link_path, target_str);
                ReadLinkRes {
                    status: Nfs4Status::Ok,
                    link: Some(target_str),
                }
            }
            Err(e) => {
                warn!("READLINK: Failed to read symlink {:?}: {}", link_path, e);
                let status = match e.kind() {
                    std::io::ErrorKind::NotFound => Nfs4Status::NoEnt,
                    std::io::ErrorKind::PermissionDenied => Nfs4Status::Access,
                    std::io::ErrorKind::InvalidInput => Nfs4Status::Inval, // Not a symlink
                    _ => Nfs4Status::Io,
                };
                ReadLinkRes {
                    status,
                    link: None,
                }
            }
        }
    }

    /// Handle PUTPUBFH operation (RFC 7862 Section 15.7)
    ///
    /// Sets current filehandle to the public filehandle.
    /// In most implementations, public FH is the same as root FH.
    pub fn handle_putpubfh(
        &self,
        _op: PutPubFhOp,
        ctx: &mut CompoundContext,
    ) -> PutPubFhRes {
        debug!("PUTPUBFH (using root FH as public FH)");

        // In most NFSv4 implementations, the public filehandle is the same as root
        // RFC 7862 Section 15.7: Public FH is rarely used in NFSv4
        match self.fh_mgr.get_root_fh() {
            Ok(fh) => {
                ctx.current_fh = Some(fh);
                PutPubFhRes {
                    status: Nfs4Status::Ok,
                }
            }
            Err(e) => {
                warn!("PUTPUBFH failed: {}", e);
                PutPubFhRes {
                    status: Nfs4Status::Resource,
                }
            }
        }
    }
    
    /// Handle GETATTR for pseudo-root (RFC 7530 Section 7)
    ///
    /// Returns synthetic attributes for the virtual root filesystem.
    async fn handle_pseudo_root_getattr(&self, op: GetAttrOp) -> GetAttrRes {
        use crate::nfs::v4::pseudo::{PSEUDO_ROOT_FSID, PSEUDO_ROOT_FILEID};
        
        let pseudo_fs = self.fh_mgr.get_pseudo_fs();
        let export_names = pseudo_fs.list_exports();
        
        // Create synthetic snapshot for pseudo-root
        let snapshot = AttributeSnapshot::pseudo_root(export_names.len());
        
        debug!("PSEUDO-ROOT GETATTR: Creating snapshot with {} exports", export_names.len());
        debug!("   FSID: {:?}", PSEUDO_ROOT_FSID);
        debug!("   FILEID: {}", PSEUDO_ROOT_FILEID);
        
        // Encode from snapshot (consistent with regular GETATTR)
        let (attr_vals, supported_bitmap) = encode_attributes_from_snapshot(
            &op.attr_request,
            &snapshot,
            self.pnfs_enabled,
            // The pseudo-root spans every volume; it advertises the
            // files-class fleet default. Per-volume refinement happens
            // at the fsid crossing into each scsi volume dir.
            None,
        );
        
        let fattr = Fattr4 {
            attrmask: supported_bitmap.clone(),
            attr_vals: attr_vals.clone(),
        };
        
        debug!("PSEUDO-ROOT GETATTR: Returning {} bytes of synthetic attributes (from snapshot)", fattr.attr_vals.len());
        
        GetAttrRes {
            status: Nfs4Status::Ok,
            obj_attributes: Some(fattr),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// A2 census (design review C5): the shared size chokepoint — the
    /// lane both SETATTR-size and OPEN-createattrs land in — must note
    /// shrink as a first-class Truncate (watermark + clip) and grow as
    /// the kernel's zero-fill of the gap.
    #[test]
    fn setattr_size_notes_tier_capture_shrink_and_grow() {
        use std::os::unix::fs::MetadataExt;
        crate::tier::capture::force_enable();
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("census-size.bin");
        std::fs::write(&path, vec![9u8; 1000]).unwrap();
        let md = std::fs::metadata(&path).unwrap();
        let (dev, ino) = (md.dev(), md.ino());
        // ext4 REUSES inode numbers: a dead test file's capture residue
        // can alias onto this identity (safe in production — pessimal
        // upload — but this test asserts EXACT capture state). Clear it.
        crate::tier::capture::forget(dev, ino);

        let want = SettableAttrs { size: Some(500), ..Default::default() };
        let (applied, err) = apply_settable_attrs(&path, &want);
        assert!(err.is_none(), "shrink failed: {err:?}");
        assert!(applied.contains(&FATTR4_SIZE));
        let cap = crate::tier::capture::snapshot(dev, ino)
            .expect("SETATTR shrink must note the tier capture (C5)");
        assert_eq!(cap.min_size, Some(500), "shrink is a first-class Truncate event");

        let want = SettableAttrs { size: Some(800), ..Default::default() };
        let (_, err) = apply_settable_attrs(&path, &want);
        assert!(err.is_none(), "grow failed: {err:?}");
        let cap = crate::tier::capture::snapshot(dev, ino).unwrap();
        assert_eq!(
            cap.intervals,
            vec![(500, 800)],
            "grow must dirty the zero-filled gap"
        );
        assert_eq!(cap.min_size, Some(500), "the watermark survives the regrow");
    }

    /// READDIR pagination must not drop entries when the directory is
    /// mutated between calls **within the same wall-clock second**.
    ///
    /// The shipped design made this unreachable-to-detect: the cookie was
    /// a positional index into a fresh re-enumeration, and `cookieverf`
    /// was `mtime.as_secs()`. A delete before the client's resume point
    /// shifts every later index down by one, while a second-granularity
    /// verifier still says "unchanged" — so the server resumes at a stale
    /// index and an entry is skipped with NFS4_OK and no error anywhere.
    /// A listing of a busy directory silently returns fewer files than it
    /// contains, which reads to an application as "the file isn't there".
    ///
    /// The directory mtime is pinned to a fixed value across the mutation
    /// so the same-second collision is deterministic rather than a race.
    #[tokio::test]
    async fn readdir_does_not_skip_entries_when_dir_changes_in_one_second() {
        use filetime::{set_file_mtime, FileTime};

        let (handler, temp) = create_test_handler();
        let dir = temp.path();
        for i in 0..60 {
            std::fs::write(dir.join(format!("f{i:02}")), b"x").unwrap();
        }
        // Pin mtime so both calls compute an identical second-granularity
        // cookieverf no matter how fast the test runs.
        let pinned = FileTime::from_unix_time(1_700_000_000, 0);
        set_file_mtime(dir, pinned).unwrap();

        let mut ctx = CompoundContext::new(0);
        handler.handle_putrootfh(PutRootFhOp, &mut ctx);

        // First page: ask for a small maxcount so the server must paginate.
        let first = handler
            .handle_readdir(
                ReadDirOp {
                    cookie: 0,
                    cookieverf: 0,
                    dircount: 256,
                    maxcount: 256,
                    attr_request: vec![],
                },
                &ctx,
            )
            .await;
        assert_eq!(first.status, Nfs4Status::Ok);
        assert!(!first.entries.is_empty(), "first page must return entries");
        assert!(!first.eof, "test needs a directory that spans >1 READDIR");

        let resume = first.entries.last().unwrap().cookie;
        let mut seen: Vec<String> = first.entries.iter().map(|e| e.name.clone()).collect();

        // Mutate: remove an entry the client has ALREADY been given. This
        // shifts the index space under the outstanding cookie.
        std::fs::remove_file(dir.join(&seen[0])).unwrap();
        // Same wall-clock SECOND, different nanoseconds — exactly what a
        // real mutation looks like. The shipped verifier truncated to
        // seconds and so could not tell this apart from "unchanged".
        set_file_mtime(dir, FileTime::from_unix_time(1_700_000_000, 500_000_000)).unwrap();

        // Resume with the cookie and verifier the server just handed out.
        let second = handler
            .handle_readdir(
                ReadDirOp {
                    cookie: resume,
                    cookieverf: first.cookieverf,
                    dircount: 65536,
                    maxcount: 65536,
                    attr_request: vec![],
                },
                &ctx,
            )
            .await;

        // Either answer is legal: finish the listing correctly, or refuse
        // with NFS4ERR_NOT_SAME so the client restarts. What is NOT legal
        // is answering OK while silently omitting a file that was present
        // the whole time.
        if second.status == Nfs4Status::NotSame {
            return; // correct: the client will restart the listing
        }
        assert_eq!(second.status, Nfs4Status::Ok);
        seen.extend(second.entries.iter().map(|e| e.name.clone()));

        let survivors: std::collections::HashSet<String> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        let seen_set: std::collections::HashSet<String> = seen.iter().cloned().collect();
        let missed: Vec<&String> = survivors.difference(&seen_set).collect();
        assert!(
            missed.is_empty(),
            "READDIR answered OK but silently skipped {} file(s) that existed \
             for the whole listing: {:?} (returned {} names for {} survivors)",
            missed.len(),
            missed,
            seen_set.len(),
            survivors.len()
        );
        // Duplicates matter as much as omissions, and this assert exists
        // because its absence let a real regression through: an earlier
        // cut of the stable-cookie work paged forever over the same
        // entries on a platform whose cookies do not survive re-open, and
        // a skip-only oracle called that a pass.
        assert_eq!(
            seen.len(),
            seen_set.len(),
            "READDIR returned duplicate entries across pages: {} names, {} unique",
            seen.len(),
            seen_set.len()
        );
    }

    /// A full paginated listing must TERMINATE and visit each entry
    /// exactly once, with no mutation involved at all.
    ///
    /// This is the plain-vanilla property; it is separated from the
    /// mutation test above because a cookie scheme can satisfy one and
    /// not the other. A cookie that does not advance loops here while
    /// skipping nothing.
    #[tokio::test]
    async fn readdir_pagination_terminates_and_visits_each_entry_once() {
        let (handler, temp) = create_test_handler();
        let dir = temp.path();
        for i in 0..40 {
            std::fs::write(dir.join(format!("p{i:02}")), b"x").unwrap();
        }
        let mut ctx = CompoundContext::new(0);
        handler.handle_putrootfh(PutRootFhOp, &mut ctx);

        let mut seen: Vec<String> = Vec::new();
        let mut cookie = 0u64;
        let mut verf = 0u64;
        // A correct listing needs a handful of pages; the cap is a
        // loop-breaker so a non-advancing cookie fails loudly instead of
        // hanging the suite.
        let mut pages = 0;
        loop {
            pages += 1;
            assert!(pages <= 50, "pagination did not terminate after 50 pages — \
                    cookie is not advancing (saw {} names)", seen.len());
            let res = handler
                .handle_readdir(
                    ReadDirOp {
                        cookie,
                        cookieverf: verf,
                        dircount: 256,
                        maxcount: 256,
                        attr_request: vec![],
                    },
                    &ctx,
                )
                .await;
            assert_eq!(res.status, Nfs4Status::Ok, "page {pages} failed");
            assert!(
                !res.entries.is_empty() || res.eof,
                "a non-eof page returned zero entries — the client would spin"
            );
            seen.extend(res.entries.iter().map(|e| e.name.clone()));
            if res.eof {
                break;
            }
            cookie = res.entries.last().unwrap().cookie;
            verf = res.cookieverf;
        }

        let unique: std::collections::HashSet<&String> = seen.iter().collect();
        assert_eq!(unique.len(), 40, "every entry exactly once; got {} unique", unique.len());
        assert_eq!(seen.len(), 40, "no entry repeated across pages; got {}", seen.len());
        assert!(pages > 1, "test needs a listing that actually paginates");
    }

    fn create_test_handler() -> (FileOperationHandler, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let export_path = temp_dir.path().to_path_buf();
        let fh_mgr = Arc::new(FileHandleManager::new(export_path));
        let handler = FileOperationHandler::new(fh_mgr, false); // false = standalone mode (no pNFS)
        (handler, temp_dir)
    }

    #[test]
    fn test_putrootfh() {
        let (handler, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);

        let res = handler.handle_putrootfh(PutRootFhOp, &mut ctx);
        assert_eq!(res.status, Nfs4Status::Ok);
        assert!(ctx.current_fh.is_some());
    }

    #[test]
    fn test_getfh() {
        let (handler, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);

        // No current FH
        let res = handler.handle_getfh(GetFhOp, &ctx);
        assert_eq!(res.status, Nfs4Status::NoFileHandle);

        // Set root FH
        handler.handle_putrootfh(PutRootFhOp, &mut ctx);

        // Get FH
        let res = handler.handle_getfh(GetFhOp, &ctx);
        assert_eq!(res.status, Nfs4Status::Ok);
        assert!(res.filehandle.is_some());
    }

    #[test]
    fn test_savefh_restorefh() {
        let (handler, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);

        // Set root FH
        handler.handle_putrootfh(PutRootFhOp, &mut ctx);
        let root_fh = ctx.current_fh.clone();

        // Save FH
        let res = handler.handle_savefh(SaveFhOp, &mut ctx);
        assert_eq!(res.status, Nfs4Status::Ok);
        assert_eq!(ctx.saved_fh, root_fh);

        // Clear current FH
        ctx.current_fh = None;

        // Restore FH
        let res = handler.handle_restorefh(RestoreFhOp, &mut ctx);
        assert_eq!(res.status, Nfs4Status::Ok);
        assert_eq!(ctx.current_fh, root_fh);
    }

    #[tokio::test]
    async fn test_access() {
        let (handler, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);

        // Set root FH
        handler.handle_putrootfh(PutRootFhOp, &mut ctx);

        // Check access
        let op = AccessOp {
            access: ACCESS4_READ | ACCESS4_LOOKUP,
        };

        let res = handler.handle_access(op, &ctx).await;
        assert_eq!(res.status, Nfs4Status::Ok);
        assert_ne!(res.access, 0);
    }

    /// Encode a fattr4 value stream the way a client would: values packed
    /// in ascending attr-number order.
    fn fattr_bytes(parts: &[&[u8]]) -> Vec<u8> {
        parts.concat()
    }

    #[test]
    fn settable_attrs_decode_mode_only() {
        // chmod 0644: bitmap [0, 1<<(33-32)], value = u32 mode. This wire
        // shape is exactly the one the old stub misread — it took the
        // bitmap length word (2) as the mode, so every chmod → 0o002.
        let attrs = decode_settable_attrs(&[0, 0x2], &0o644u32.to_be_bytes()).unwrap();
        assert_eq!(attrs.mode, Some(0o644));
        assert_eq!(attrs.size, None);
        assert_eq!(attrs.atime, None);
        assert_eq!(attrs.mtime, None);
    }

    #[test]
    fn settable_attrs_decode_size_mode_and_times() {
        // SIZE(4) + MODE(33) + TIME_ACCESS_SET(48, server) +
        // TIME_MODIFY_SET(54, client 1234.5678) — values in attr order.
        let mask = [1u32 << 4, (1 << 1) | (1 << 16) | (1 << 22)];
        let vals = fattr_bytes(&[
            &4096u64.to_be_bytes(),      // size
            &0o600u32.to_be_bytes(),     // mode
            &0u32.to_be_bytes(),         // atime: SET_TO_SERVER_TIME4
            &1u32.to_be_bytes(),         // mtime: SET_TO_CLIENT_TIME4
            &1234i64.to_be_bytes(),      //   seconds
            &5678u32.to_be_bytes(),      //   nseconds
        ]);
        let attrs = decode_settable_attrs(&mask, &vals).unwrap();
        assert_eq!(attrs.size, Some(4096));
        assert_eq!(attrs.mode, Some(0o600));
        assert_eq!(attrs.atime, Some(SetTime::ServerTime));
        assert_eq!(attrs.mtime, Some(SetTime::ClientTime { seconds: 1234, nseconds: 5678 }));
    }

    #[test]
    fn settable_attrs_decode_owner_parsed_and_aligned() {
        // OWNER(36) precedes TIME_MODIFY_SET(54); its opaque bytes must be
        // consumed (with XDR padding) or the time decodes garbage — and
        // the numeric id (with optional @domain suffix) lands in `owner`.
        let mask = [0u32, (1 << 4) | (1 << 22)];
        let vals = fattr_bytes(&[
            &5u32.to_be_bytes(), b"1000@\0\0\0"[..8].as_ref(), // owner "1000@" + pad to 8
            &0u32.to_be_bytes(),                                // mtime: server time
        ]);
        let attrs = decode_settable_attrs(&mask, &vals).unwrap();
        assert_eq!(attrs.owner, Some(1000));
        assert_eq!(attrs.mtime, Some(SetTime::ServerTime));
        assert_eq!(attrs.mode, None);
    }

    #[test]
    fn owner4_parse_forms() {
        assert_eq!(parse_owner4(b"999"), Ok(999));
        assert_eq!(parse_owner4(b"999@localdomain"), Ok(999));
        assert_eq!(parse_owner4(b"0"), Ok(0));
        assert_eq!(parse_owner4(b"postgres"), Err(Nfs4Status::BadOwner));
        assert_eq!(parse_owner4(b""), Err(Nfs4Status::BadOwner));
    }

    #[test]
    #[cfg(unix)]
    fn apply_settable_attrs_chown_to_self() {
        // chown to the current euid/egid is permitted unprivileged — the
        // apply path must report OWNER/OWNER_GROUP as applied.
        use std::os::unix::fs::MetadataExt;
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("f");
        std::fs::write(&path, b"x").unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let want = SettableAttrs {
            owner: Some(meta.uid()),
            owner_group: Some(meta.gid()),
            ..Default::default()
        };
        let (applied, err) = apply_settable_attrs(&path, &want);
        assert_eq!(err, None);
        assert!(applied.contains(&FATTR4_OWNER));
        assert!(applied.contains(&FATTR4_OWNER_GROUP));
    }

    #[test]
    fn settable_attrs_reject_readonly_and_unsupported() {
        // TYPE (attr 1) is read-only → INVAL.
        assert_eq!(
            decode_settable_attrs(&[1 << 1], &2u32.to_be_bytes()).unwrap_err(),
            Nfs4Status::Inval
        );
        // ACL (attr 12) is writable-but-unsupported → ATTRNOTSUPP.
        assert_eq!(
            decode_settable_attrs(&[1 << 12], &0u32.to_be_bytes()).unwrap_err(),
            Nfs4Status::AttrNotsupp
        );
        // Truncated value stream → BADXDR.
        assert_eq!(
            decode_settable_attrs(&[0, 0x2], &[0u8; 2]).unwrap_err(),
            Nfs4Status::BadXdr
        );
    }

    #[test]
    #[cfg(unix)]
    fn apply_settable_attrs_chmod_truncate_times() {
        use std::os::unix::fs::PermissionsExt;
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("f");
        std::fs::write(&path, b"hello world").unwrap();

        let want = SettableAttrs {
            size: Some(5),
            mode: Some(0o640),
            owner: None,
            owner_group: None,
            atime: None,
            mtime: Some(SetTime::ClientTime { seconds: 1_000_000, nseconds: 0 }),
        };
        let (applied, err) = apply_settable_attrs(&path, &want);
        assert_eq!(err, None);
        let mut sorted = applied.clone();
        sorted.sort();
        assert_eq!(sorted, vec![FATTR4_SIZE, FATTR4_MODE, FATTR4_TIME_MODIFY_SET]);

        let meta = path.metadata().unwrap();
        assert_eq!(meta.len(), 5);
        assert_eq!(meta.permissions().mode() & 0o7777, 0o640);
        assert_eq!(
            meta.modified().unwrap(),
            std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000)
        );

        // Truncating a directory reports ISDIR and applies nothing else.
        let (applied, err) = apply_settable_attrs(
            temp.path(),
            &SettableAttrs { size: Some(0), ..Default::default() },
        );
        assert_eq!(err, Some(Nfs4Status::IsDir));
        assert!(applied.is_empty());
    }

    #[test]
    fn attr_bitmap_roundtrip() {
        assert_eq!(attr_numbers_to_bitmap(&[]), Vec::<u32>::new());
        assert_eq!(
            attr_numbers_to_bitmap(&[FATTR4_SIZE, FATTR4_MODE, FATTR4_TIME_MODIFY_SET]),
            vec![1 << 4, (1 << 1) | (1 << 22)]
        );
    }

    /// A7 wiring: a rename-over through the REAL handler queues the
    /// identity event — the covered file's generation tombstones, the
    /// moved file's bit re-points at the new path. The event queue is
    /// process-global (parallel tests can steal a drain), so each
    /// retry re-runs a real rename in the alternating direction and
    /// re-seeds the covered row until OUR backend shows the result.
    #[tokio::test]
    async fn rename_over_tombstones_covered_generation() {
        // Queues and/or drains the PROCESS-GLOBAL capture queue.
        // Held for the whole body: the theft window is queue-to-drain,
        // not the drain alone. See `capture::test_exclusive`.
        let _excl = crate::tier::capture::test_exclusive();
        use crate::state_backend::{StateBackend, TierGenerationRow};
        use std::os::unix::fs::MetadataExt;
        crate::tier::capture::force_enable();
        let (handler, temp) = create_test_handler();
        let be: std::sync::Arc<dyn StateBackend> =
            std::sync::Arc::new(crate::state_backend::memory::MemoryBackend::new());
        let mut ctx = CompoundContext::new(0);
        assert_eq!(handler.handle_putrootfh(PutRootFhOp, &mut ctx).status, Nfs4Status::Ok);
        ctx.saved_fh = ctx.current_fh.clone();

        let names = ["ren-a.bin", "ren-b.bin"];
        let mut landed = false;
        for round in 0..50 {
            let (src, dst) = (names[round % 2], names[(round + 1) % 2]);
            std::fs::write(temp.path().join(src), format!("gen {}", round)).unwrap();
            if !temp.path().join(dst).exists() {
                std::fs::write(temp.path().join(dst), b"covered").unwrap();
            }
            let cov_md = std::fs::metadata(temp.path().join(dst)).unwrap();
            let cov = (cov_md.dev(), cov_md.ino());
            let cov_key = format!("t/{}", dst);
            be.tier_upsert_generation(&TierGenerationRow {
                dev: cov.0,
                ino: cov.1,
                key: cov_key.clone(),
                generation: 1,
                etag: "\"cov-etag\"".into(),
                crc64_b64: None,
                size: 7,
                copy_allowed: true,
                updated_unix: 1,
            })
            .await
            .unwrap();

            let res = handler
                .handle_rename(
                    RenameOp { oldname: src.to_string(), newname: dst.to_string() },
                    &ctx,
                )
                .await;
            assert_eq!(res.status, Nfs4Status::Ok);
            let _ = crate::tier::durable::drain_pending(&be).await;

            let tombstoned = be
                .tier_list_tombstones()
                .await
                .unwrap()
                .iter()
                .any(|t| t.key == cov_key && t.etag.as_deref() == Some("\"cov-etag\""));
            let moved_repointed = be.tier_list_dirty().await.unwrap().iter().any(|r| {
                r.path
                    .as_deref()
                    .is_some_and(|pth| pth.ends_with(dst))
            });
            if tombstoned && moved_repointed {
                landed = true;
                break;
            }
        }
        assert!(landed, "the rename's identity event never landed in our backend");
    }

    /// Step 10 (C2): GETATTR of an evicted file serves the LOGICAL
    /// A file created and never written must still be dirty, or it
    /// never becomes a bucket object: the flush has nothing to publish,
    /// the manifest cannot list it as restorable, and `rpoClean` reads
    /// true while the file exists only on the PVC. Hibernate then
    /// deletes the PVC and `touch .gitkeep` is gone for good.
    #[tokio::test]
    async fn a_created_but_never_written_file_is_dirty() {
        use std::os::unix::fs::MetadataExt;
        crate::tier::capture::force_enable();
        let (handler, temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);
        ctx.current_fh = Some(handler.fh_mgr.get_or_create_handle(temp.path()).unwrap());

        let res = handler
            .handle_create(
                CreateOp {
                    objtype: Nfs4FileType::Regular,
                    objname: "gitkeep".to_string(),
                    linkdata: None,
                    createattrs: Fattr4 { attrmask: vec![], attr_vals: vec![] },
                },
                &mut ctx,
            )
            .await;
        assert_eq!(res.status, Nfs4Status::Ok);

        let md = std::fs::metadata(temp.path().join("gitkeep")).unwrap();
        let cap = crate::tier::capture::snapshot(md.dev(), md.ino())
            .expect("a fresh create must leave a capture entry");
        assert!(cap.is_dirty(), "the birth of a file is a mutation the tier must publish");
    }

    /// size from the marker — the 0-byte stub would read as
    /// truncation.
    #[test]
    fn snapshot_serves_logical_size_for_evicted_files() {
        use std::os::unix::fs::MetadataExt;
        crate::tier::capture::force_enable();
        let dir = tempfile::TempDir::new().unwrap();
        let f = dir.path().join("stub.bin");
        std::fs::write(&f, b"").unwrap();
        let md = std::fs::metadata(&f).unwrap();
        let (dev, ino) = (md.dev(), md.ino());
        crate::tier::capture::forget(dev, ino);
        crate::tier::evict::install_marker_for_tests(dev, ino, 4242);
        let snap = AttributeSnapshot::from_metadata(std::fs::metadata(&f).unwrap(), &f).unwrap();
        assert_eq!(snap.size, 4242, "logical size from the marker");
        crate::tier::evict::forget(dev, ino);
        let snap = AttributeSnapshot::from_metadata(std::fs::metadata(&f).unwrap(), &f).unwrap();
        assert_eq!(snap.size, 0, "physical size once the marker clears");
    }

    /// A10: with the space gauge live, the pseudo-root SPACE_* arms
    /// (where the mount root's statfs lands) serve the PVC's real
    /// statvfs numbers instead of the historical 8 EiB. Retry loop
    /// absorbs a concurrent test swapping the global install.
    #[test]
    fn pseudo_root_space_attrs_read_the_pvc_when_the_gauge_is_live() {
        let dir = tempfile::TempDir::new().unwrap();
        let scfg = crate::tier::space::SpaceConfig {
            root: dir.path().to_path_buf(),
            reserve_bytes: 0,
            watermark_pct: 85,
            ballast_path: None,
            ballast_bytes: 0,
        };
        let attrs = crate::nfs::v4::pseudo::PseudoRootAttrs {
            fsid: (0, 0),
            fileid: 2,
            nlink: 2,
            size: 4096,
            create_time: 0,
            instance_id: 1,
        };
        let mut ok = false;
        for _ in 0..50 {
            crate::tier::space::configure(scfg.clone()).unwrap();
            let Some(v) = crate::tier::space::view() else { continue };
            let mut buf = BytesMut::new();
            assert!(encode_pseudo_root_attribute(FATTR4_SPACE_TOTAL, &attrs, &mut buf, false));
            let got = u64::from_be_bytes(buf[..8].try_into().unwrap());
            if got == v.total_bytes && got > 0 && got != u64::MAX / 2 {
                ok = true;
                break;
            }
        }
        assert!(ok, "SPACE_TOTAL must serve the real PVC size, not the 8 EiB constant");
    }
}
