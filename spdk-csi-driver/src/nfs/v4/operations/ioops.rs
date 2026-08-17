// NFSv4 I/O Operations
//
// This module implements file I/O operations for NFSv4:
// - OPEN: Open a file and receive stateid
// - CLOSE: Close a file and release stateid
// - READ: Read data from file
// - WRITE: Write data to file
// - COMMIT: Commit unstable writes to stable storage
//
// NFSv4 uses stateids to track open files and locks.
// Every I/O operation (READ/WRITE) requires a valid stateid.

use crate::nfs::v4::protocol::*;
use crate::nfs::v4::compound::CompoundContext;
use crate::nfs::v4::state::StateManager;
use crate::nfs::v4::operations::fileops::Fattr4;
use crate::nfs::v4::filehandle::FileHandleManager;
use bytes::Bytes;
use super::fd_cache::{CachedFile, FdCache};
use std::fs::File;
use std::path::PathBuf;
use std::sync::Arc;
use std::os::unix::fs::FileExt;
use tracing::{debug, info, warn};

/// Open claim types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenClaimType {
    Null = 0,           // CLAIM_NULL - open by name
    Previous = 1,       // CLAIM_PREVIOUS - reclaim after reboot
    DelegateCur = 2,    // CLAIM_DELEGATE_CUR - via delegation
    DelegatePrev = 3,   // CLAIM_DELEGATE_PREV - reclaim delegation
    FH = 4,             // CLAIM_FH - open by filehandle (NFSv4.1)
    DelegCurFH = 5,     // CLAIM_DELEG_CUR_FH (NFSv4.1)
    DelegPrevFH = 6,    // CLAIM_DELEG_PREV_FH (NFSv4.1)
}

/// Open delegation types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenDelegationType {
    None = 0,           // No delegation
    Read = 1,           // Read delegation
    Write = 2,          // Write delegation
}

/// OPEN operation (opcode 18)
///
/// Opens a file and returns a stateid for subsequent I/O.
pub struct OpenOp {
    /// Sequence ID (for exactly-once semantics with open-owner)
    pub seqid: u32,

    /// Share access (READ, WRITE, BOTH)
    pub share_access: u32,

    /// Share deny (NONE, READ, WRITE, BOTH)
    pub share_deny: u32,

    /// Open owner (client-provided identifier)
    pub owner: Vec<u8>,

    /// How to open (CREATE, NOCREATE)
    pub openhow: OpenHow,

    /// Claim type and value
    pub claim: OpenClaim,
}

#[derive(Debug, Clone)]
pub enum OpenHow {
    /// Don't create - file must exist
    NoCreate,

    /// Create if doesn't exist (with attributes)
    Create(Fattr4),

    /// Exclusive create (with verifier)
    Exclusive(u64),

    /// Exclusive create with attributes (NFSv4.1)
    Exclusive4_1 { verifier: u64, attrs: Fattr4 },
}

#[derive(Debug, Clone)]
pub enum OpenClaim {
    /// Open by name in current directory
    Null(String),

    /// Open by filehandle (NFSv4.1)
    Fh,
}

/// Share access bits
pub const OPEN4_SHARE_ACCESS_READ: u32 = 0x00000001;
pub const OPEN4_SHARE_ACCESS_WRITE: u32 = 0x00000002;
pub const OPEN4_SHARE_ACCESS_BOTH: u32 = 0x00000003;

/// Share deny bits
pub const OPEN4_SHARE_DENY_NONE: u32 = 0x00000000;
pub const OPEN4_SHARE_DENY_READ: u32 = 0x00000001;
pub const OPEN4_SHARE_DENY_WRITE: u32 = 0x00000002;
pub const OPEN4_SHARE_DENY_BOTH: u32 = 0x00000003;

/// OPEN result flag: server supports POSIX-semantics byte-range locks on this
/// file. The Linux kernel client returns ENOLCK without ever sending a LOCK op
/// unless this bit is set in the OPEN reply (RFC 8881 §18.16.3).
pub const OPEN4_RESULT_LOCKTYPE_POSIX: u32 = 0x00000004;

pub struct OpenRes {
    pub status: Nfs4Status,
    pub stateid: Option<StateId>,
    pub change_info: Option<ChangeInfo>,
    pub result_flags: u32,
    pub delegation: OpenDelegationType,
    pub attrset: Vec<u32>,  // Which CREATE attrs were set
}

#[derive(Debug, Clone)]
pub struct ChangeInfo {
    pub atomic: bool,
    pub before: u64,
    pub after: u64,
}

/// CLOSE operation (opcode 4)
///
/// Closes a file and releases the stateid.
pub struct CloseOp {
    pub seqid: u32,
    pub stateid: StateId,
}

pub struct CloseRes {
    pub status: Nfs4Status,
    pub stateid: Option<StateId>,
}

/// DELEGRETURN operation (opcode 8)
/// Client voluntarily returns a delegation (or after recall)
pub struct DelegReturnRes {
    pub status: Nfs4Status,
}

/// READ operation (opcode 25)
///
/// Reads data from a file.
pub struct ReadOp {
    pub stateid: StateId,
    pub offset: u64,
    pub count: u32,
}

pub struct ReadRes {
    pub status: Nfs4Status,
    pub eof: bool,
    pub data: Bytes,
}

/// WRITE operation (opcode 38)
///
/// Writes data to a file.
pub struct WriteOp {
    pub stateid: StateId,
    pub offset: u64,
    pub stable: u32,    // UNSTABLE=0, DATA_SYNC=1, FILE_SYNC=2
    pub data: Bytes,
}

/// Write stability
pub const UNSTABLE4: u32 = 0;       // May be cached
pub const DATA_SYNC4: u32 = 1;      // Committed to storage
pub const FILE_SYNC4: u32 = 2;      // Data + metadata committed

pub struct WriteRes {
    pub status: Nfs4Status,
    pub count: u32,     // Bytes written
    pub committed: u32, // Actual stability achieved
    pub writeverf: u64, // Write verifier (for COMMIT)
}

/// COMMIT operation (opcode 5)
///
/// Commits unstable writes to stable storage.
pub struct CommitOp {
    pub offset: u64,
    pub count: u32,
}

pub struct CommitRes {
    pub status: Nfs4Status,
    pub writeverf: u64,
}

/// Read-only view over the fd cache for handlers outside ioops
/// (GETATTR lives in fileops). F17b: a renamed-over/removed file whose
/// path no longer resolves is still fully alive through server-held
/// opens — POSIX unlink-open semantics. This view lets fh-only ops
/// (GETATTR carries no stateid) find such a file by its handle's
/// embedded path and serve attributes via fstat instead of STALE,
/// which is what keeps the kernel client from running a state-recovery
/// cycle after every postgres rename-over.
#[derive(Clone)]
pub struct OpenFileView {
    fd_cache: Arc<FdCache>,
}

impl OpenFileView {
    /// An open fd whose OPEN-time path equals `path`, if any. Point
    /// lookup via the cache's path index (F24 retired the scans).
    pub fn file_for_path(&self, path: &std::path::Path) -> Option<Arc<File>> {
        self.fd_cache.find_by_path(path, false).map(|e| e.file)
    }

    /// An open fd whose OPEN-time inode equals `ino`, with its
    /// OPEN-time path — the v4 kernel-handle variant of the fallback
    /// (v4 handles embed an ino but no path).
    pub fn entry_for_ino(&self, ino: u64) -> Option<(std::path::PathBuf, Arc<File>)> {
        self.fd_cache.find_by_ino(ino, false).map(|e| (e.path, e.file))
    }
}

/// Whether an fd may be cached under this stateid. Special stateids
/// (`other` all-zeros / all-ones, RFC 8881 §8.2.3) are not unique to
/// one open — caching under them would alias different files to the
/// same key and serve one file's fd for another's I/O.
fn cacheable_stateid(other: &[u8; 12]) -> bool {
    *other != [0u8; 12] && *other != [0xffu8; 12]
}

/// Whether the server may grant delegations (FLINT_NFS_DELEGATIONS=1).
/// Off by default: see the gate in `try_grant_read_delegation`.
fn delegations_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("FLINT_NFS_DELEGATIONS")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    })
}

/// I/O operation handler with file descriptor caching
pub struct IoOperationHandler {
    state_mgr: Arc<StateManager>,
    fh_mgr: Arc<FileHandleManager>,
    write_verifier: u64,
    /// File descriptor cache (guard-free API + path index; see
    /// fd_cache.rs for the F24 discipline it enforces).
    fd_cache: Arc<FdCache>,
}

impl IoOperationHandler {
    /// Create a new I/O operation handler
    pub fn new(state_mgr: Arc<StateManager>, fh_mgr: Arc<FileHandleManager>) -> Self {
        // Generate write verifier (used to detect server reboots)
        use std::time::{SystemTime, UNIX_EPOCH};
        let write_verifier = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        
        Self {
            state_mgr,
            fh_mgr,
            write_verifier,
            fd_cache: Arc::new(FdCache::new()),
        }
    }

    /// This server lifetime's write verifier — the value WRITE and COMMIT
    /// report, and therefore the value COPY must report too.
    ///
    /// A Linux client (6.8, verified on the wire) issues COPY and COMMIT
    /// in ONE compound and compares COPY's `wr_writeverf` against
    /// COMMIT's verifier. Any difference reads as "the server rebooted
    /// mid-copy, the data may be gone", and the client reissues the
    /// identical COPY — forever. Returning a constant here is not a
    /// cosmetic inaccuracy; it is an infinite loop.
    pub fn write_verifier(&self) -> u64 {
        self.write_verifier
    }

    /// Shared read-only view over the fd cache (see [`OpenFileView`]).
    pub fn open_file_view(&self) -> OpenFileView {
        OpenFileView {
            fd_cache: Arc::clone(&self.fd_cache),
        }
    }

    /// Test-only: seed the fd cache as if an OPEN had cached this fd.
    #[cfg(test)]
    pub(crate) fn test_seed_fd(
        &self,
        other: [u8; 12],
        file: Arc<File>,
        path: PathBuf,
        writable: bool,
    ) {
        let ino = CachedFile::ino_of(&file);
        self.fd_cache.insert(other, CachedFile { file, path, writable, ino });
    }

    /// F17c: anchor the open — cache an fd under the open stateid AT
    /// OPEN TIME, so the file keeps serving across rename-over even
    /// before any READ/WRITE reaches it (knfsd's "an open holds the
    /// file" invariant; postgres backends OPEN pg_internal.init and a
    /// concurrent regeneration renames it over before the READ lands).
    /// `allow_fresh_open=false` for stale-resolved CLAIM_FH re-opens:
    /// those must only REUSE an fd of the original inode — fresh-opening
    /// the path would alias the NEW file under the old handle.
    /// Residual: fds of clients that die without CLOSE outlive the
    /// state entries (lease sweep doesn't reach this cache yet).
    fn seed_open_fd(&self, stateid: &StateId, path: &PathBuf, allow_fresh_open: bool) {
        if !cacheable_stateid(&stateid.other) {
            return;
        }
        if self.fd_cache.contains(&stateid.other) {
            return;
        }
        // Point lookup via the path index; FdCache's API never hands
        // out a guard, so the F24 iter-guard-across-insert deadlock is
        // structurally impossible here.
        if let Some(existing) = self.fd_cache.find_by_path(path, false) {
            self.fd_cache.insert(
                stateid.other,
                CachedFile {
                    file: existing.file,
                    path: path.clone(),
                    writable: existing.writable,
                    ino: existing.ino,
                },
            );
            return;
        }
        if !allow_fresh_open {
            return;
        }
        let opened = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map(|f| (f, true))
            .or_else(|_| std::fs::File::open(path).map(|f| (f, false)));
        if let Ok((f, writable)) = opened {
            let file = Arc::new(f);
            let ino = CachedFile::ino_of(&file);
            self.fd_cache.insert(
                stateid.other,
                CachedFile { file, path: path.clone(), writable, ino },
            );
        }
    }

    /// F17b fallback for READ/WRITE when the filehandle no longer
    /// resolves (object renamed-over or removed): the handle's embedded
    /// path names the ORIGINAL file, which is still alive if any open
    /// fd targets it. Returns (embedded path, cached fd for this
    /// stateid or any other open of the same path).
    fn stale_open_fallback(
        &self,
        fh: &crate::nfs::v4::protocol::Nfs4FileHandle,
        stateid_other: &[u8; 12],
        want_writable: bool,
    ) -> Option<(PathBuf, Arc<File>)> {
        // v1/v3 handles embed the original path; v4 kernel handles
        // embed only the ino (no path exists to extract). Either key
        // finds the anchored open file.
        let embedded = FileHandleManager::parse_path_lenient(fh).ok();
        let ino = FileHandleManager::object_ino(fh);
        if let Some(e) = self.fd_cache.get(stateid_other) {
            let same_object = embedded.as_ref().is_some_and(|p| e.path == *p)
                || ino.is_some_and(|i| i != 0 && e.ino == i);
            if same_object && (!want_writable || e.writable) {
                return Some((e.path, e.file));
            }
        }
        if let Some(p) = embedded {
            let hit = self.fd_cache.find_by_path(&p, want_writable);
            if let Some(e) = hit {
                return Some((p, e.file));
            }
        }
        let hit = ino.and_then(|i| self.fd_cache.find_by_ino(i, want_writable));
        hit.map(|e| (e.path, e.file))
    }

    /// Get client ID from compound context
    ///
    /// Looks up the session (set by SEQUENCE) to determine the client ID.
    /// Falls back to 1 for backward compatibility with tests that don't use SEQUENCE.
    fn get_client_id_from_context(&self, ctx: &CompoundContext) -> u64 {
        if let Some(session_id) = &ctx.session_id {
            if let Some(session) = self.state_mgr.sessions.get_session(session_id) {
                return session.client_id;
            }
            warn!("OPEN: Session {:?} not found in context, falling back to client_id=1", session_id);
        } else {
            debug!("OPEN: No session in context (likely test), using client_id=1");
        }
        1 // Fallback for tests
    }

    /// Handle OPEN operation
    pub async fn handle_open(
        &self,
        op: OpenOp,
        ctx: &mut CompoundContext,
    ) -> OpenRes {
        debug!("OPEN: share_access=0x{:08x}, share_deny=0x{:08x}",
               op.share_access, op.share_deny);
        debug!("OPEN: openhow={:?}, claim={:?}", op.openhow, op.claim);

        // Check current filehandle (directory we're creating in).
        // Clone it so we don't keep an immutable borrow on `ctx`
        // through the rest of the handler — the no-create path
        // updates CFH to the file's fh after resolving the name,
        // and a long-lived `&ctx.current_fh` would block that.
        let current_fh = match &ctx.current_fh {
            Some(fh) => fh.clone(),
            None => {
                return OpenRes {
                    status: Nfs4Status::NoFileHandle,
                    stateid: None,
                    change_info: None,
                    result_flags: 0,
                    delegation: OpenDelegationType::None,
                    attrset: vec![],
                };
            }
        };
        let current_fh = &current_fh;

        // Extract filename from claim
        let filename = match &op.claim {
            OpenClaim::Null(name) => {
                if let Some(status) =
                    crate::nfs::v4::operations::fileops::validate_component_name(name)
                {
                    warn!("OPEN: invalid claim name → {:?}", status);
                    return OpenRes {
                        status,
                        stateid: None,
                        change_info: None,
                        result_flags: 0,
                        delegation: OpenDelegationType::None,
                        attrset: vec![],
                    };
                }
                name.clone()
            }
            OpenClaim::Fh => {
                // CLAIM_FH - opening by filehandle, file must exist
                debug!("OPEN: CLAIM_FH - file must exist");
                String::new()
            }
        };

        // Determine if we need to create the file
        let should_create = !matches!(op.openhow, OpenHow::NoCreate);
        
        if should_create && !filename.is_empty() {
            // Create the file
            debug!("OPEN: Creating file '{}'", filename);

            // Resolve parent directory path
            let parent_path = match self.fh_mgr.resolve_handle(current_fh) {
                Ok(p) => p,
                Err(e) => {
                    warn!("OPEN: Failed to resolve parent directory: {}", e);
                    return OpenRes {
                        status: Nfs4Status::Stale,
                        stateid: None,
                        change_info: None,
                        result_flags: 0,
                        delegation: OpenDelegationType::None,
                        attrset: vec![],
                    };
                }
            };

            // Build full file path
            let file_path = parent_path.join(&filename);
            debug!("OPEN: Creating file at {:?}", file_path);

            // Extract verifier for EXCLUSIVE4 / EXCLUSIVE4_1 paths.
            // Per RFC 8881 §18.16.5 the verifier is the client's
            // dedupe key on retry: same verifier on an existing file
            // returns the original stateid; different verifier on an
            // existing file returns `NFS4ERR_EXIST`.
            let exclusive_verifier: Option<u64> = match &op.openhow {
                OpenHow::Exclusive(v) => Some(*v),
                OpenHow::Exclusive4_1 { verifier, .. } => Some(*verifier),
                _ => None,
            };

            // RFC 8881 §18.16.5 EXCLUSIVE retry: if the file already
            // exists, look up any prior exclusive-create on it. Same
            // verifier → return existing stateid (idempotent retry).
            // Different verifier → NFS4ERR_EXIST.
            if let Some(verifier) = exclusive_verifier {
                if file_path.exists() {
                    if let Ok(existing_fh) = self.fh_mgr.path_to_filehandle(&file_path) {
                        match self
                            .state_mgr
                            .stateids
                            .find_exclusive_match(&existing_fh.data, verifier)
                        {
                            Some(_) => {
                                // Idempotent retry — fall through to no-create
                                // path which will resolve to the existing fh
                                // and bump seqid via record_open below.
                                debug!(
                                    "OPEN(EXCLUSIVE4): retry with matching verifier {:#x} → existing stateid",
                                    verifier
                                );
                            }
                            None => {
                                warn!(
                                    "OPEN(EXCLUSIVE4): file exists with non-matching verifier {:#x} → EXIST",
                                    verifier
                                );
                                return OpenRes {
                                    status: Nfs4Status::Exist,
                                    stateid: None,
                                    change_info: None,
                                    result_flags: 0,
                                    delegation: OpenDelegationType::None,
                                    attrset: vec![],
                                };
                            }
                        }
                    }
                }
            }

            // Decode the settable subset of createattrs up front — a
            // malformed or unsupported fattr4 must fail the OPEN before
            // any filesystem mutation (RFC 8881 §18.16.4).
            use crate::nfs::v4::operations::fileops::{
                apply_settable_attrs_offloaded, attr_numbers_to_bitmap, decode_settable_attrs,
                SettableAttrs,
            };
            let createattrs = match &op.openhow {
                OpenHow::Create(attrs) | OpenHow::Exclusive4_1 { attrs, .. } => {
                    match decode_settable_attrs(&attrs.attrmask, &attrs.attr_vals) {
                        Ok(d) => Some(d),
                        Err(status) => {
                            warn!("OPEN(create): undecodable createattrs → {:?}", status);
                            return OpenRes {
                                status,
                                stateid: None,
                                change_info: None,
                                result_flags: 0,
                                delegation: OpenDelegationType::None,
                                attrset: vec![],
                            };
                        }
                    }
                }
                _ => None,
            };

            // Create-but-don't-truncate: UNCHECKED4 on an existing file
            // must leave its data alone unless the client asked for
            // size=0 via createattrs (RFC 8881 §18.16.3). File::create's
            // implicit O_TRUNC would wipe it.
            let existed = tokio::fs::try_exists(&file_path).await.unwrap_or(false);
            // A10 admission: refuse NEW-file creation with NOSPC past
            // the reserve. Opening an EXISTING file always proceeds —
            // reads must flow at any fullness.
            if !existed && crate::tier::space::admit_create(&file_path).is_err() {
                warn!("OPEN(create): refused NOSPC — PVC headroom-minus-reserve exhausted");
                return OpenRes {
                    status: Nfs4Status::NoSpc,
                    stateid: None,
                    change_info: None,
                    result_flags: 0,
                    delegation: OpenDelegationType::None,
                    attrset: vec![],
                };
            }
            // read+write: this fd is seeded into the fd-cache below and
            // must serve BOTH directions (a write-only fd turns a later
            // READ through the cache into EBADF).
            match tokio::fs::OpenOptions::new().read(true).write(true).create(true).open(&file_path).await {
                Ok(created) => {
                    debug!(
                        "OPEN: {} file {:?}",
                        if existed { "opened existing" } else { "created" },
                        file_path
                    );

                    // Apply createattrs: everything on a fresh create;
                    // only the size request (O_TRUNC) when the file
                    // already existed — other attrs are ignored then.
                    let mut applied_attrs: Vec<u32> = Vec::new();
                    if let Some(want) = &createattrs {
                        let effective = if existed {
                            SettableAttrs { size: want.size, ..Default::default() }
                        } else {
                            want.clone()
                        };
                        let (applied, err) =
                            apply_settable_attrs_offloaded(file_path.clone(), effective).await;
                        applied_attrs = applied;
                        if let Some(status) = err {
                            warn!("OPEN(create): applying createattrs failed → {:?}", status);
                            return OpenRes {
                                status,
                                stateid: None,
                                change_info: None,
                                result_flags: 0,
                                delegation: OpenDelegationType::None,
                                attrset: attr_numbers_to_bitmap(&applied_attrs),
                            };
                        }
                    }

                    // F14: a fresh create mutates the file (birth) and the
                    // parent directory (new dirent) — both change attrs
                    // must outrun colliding ctime ticks.
                    if !existed {
                        crate::nfs::v4::change_counter::bump_path(&file_path);
                        if let Some(parent) = file_path.parent() {
                            crate::nfs::v4::change_counter::bump_path(parent);
                        }
                    }

                    // Fresh create without an explicit createattrs owner:
                    // stamp the caller's AUTH_SYS identity on the file —
                    // without this every file lands owned by the server
                    // process (root) and ownership-sensitive workloads
                    // (postgres checks st_uid == geteuid) refuse to run.
                    // Best effort: a chown failure must not fail the OPEN.
                    if !existed {
                        let (expl_uid, expl_gid) = createattrs
                            .as_ref()
                            .map(|w| (w.owner, w.owner_group))
                            .unwrap_or((None, None));
                        if let Some((uid, gid)) = ctx.unix_cred {
                            let want_uid = if expl_uid.is_none() { Some(uid) } else { None };
                            let want_gid = if expl_gid.is_none() { Some(gid) } else { None };
                            if want_uid.is_some() || want_gid.is_some() {
                                let p = file_path.clone();
                                let _ = tokio::task::spawn_blocking(move || {
                                    std::os::unix::fs::chown(&p, want_uid, want_gid)
                                })
                                .await;
                            }
                        }
                    }

                    // Generate filehandle for the new file
                    match self.fh_mgr.path_to_filehandle(&file_path) {
                        Ok(new_fh) => {
                            debug!("OPEN: Generated filehandle for new file");
                            // Update current filehandle to the newly created file
                            ctx.set_current_fh(new_fh.clone());

                            // Get client ID from session (set by SEQUENCE operation)
                            let client_id = self.get_client_id_from_context(ctx);

                            // RFC 8881 §9.7 share-deny conflict on the
                            // create path. Courtesy-cleanup at the top
                            // of dispatch_compound has already swept
                            // expired clients' open-state, so this gate
                            // now only fires on live conflicts.
                            if self.state_mgr.stateids.share_conflict(
                                &new_fh.data,
                                client_id,
                                &op.owner,
                                op.share_access,
                                op.share_deny,
                            ) {
                                warn!(
                                    "OPEN(create): share-deny conflict on {:?} → SHARE_DENIED",
                                    new_fh.data
                                );
                                return OpenRes {
                                    status: Nfs4Status::ShareDenied,
                                    stateid: None,
                                    change_info: None,
                                    result_flags: 0,
                                    delegation: OpenDelegationType::None,
                                    attrset: vec![],
                                };
                            }

                            // Record the open (RFC 7530 §16.16 — same
                            // (client, owner, fh) on a follow-on OPEN
                            // bumps seqid and merges share-masks).
                            let stateid = self.state_mgr.stateids.record_open(
                                client_id,
                                op.owner.clone(),
                                new_fh.data.clone(),
                                op.share_access,
                                op.share_deny,
                                exclusive_verifier,
                            );

                            debug!("OPEN: stateid {:?} for client {}", stateid, client_id);

                            // F17c: anchor the open with an fd immediately —
                            // and prefer the CREATE's OWN fd over a fresh
                            // open. The create's fd is writable regardless
                            // of the mode the file was born with (git
                            // creates loose objects 0444 at birth, then
                            // flushes through the open); a fresh open of a
                            // 0444 file can only ever be read-only, and the
                            // close-time flush would die EIO.
                            if cacheable_stateid(&stateid.other)
                                && !self.fd_cache.contains(&stateid.other)
                            {
                                let file = Arc::new(created.into_std().await);
                                let ino = CachedFile::ino_of(&file);
                                self.fd_cache.insert(
                                    stateid.other,
                                    CachedFile {
                                        file,
                                        path: file_path.clone(),
                                        writable: true,
                                        ino,
                                    },
                                );
                            } else {
                                self.seed_open_fd(&stateid, &file_path, true);
                            }

                            return OpenRes {
                                status: Nfs4Status::Ok,
                                stateid: Some(stateid),
                                change_info: Some(ChangeInfo {
                                    atomic: true,
                                    before: 0,
                                    after: 1,
                                }),
                                result_flags: OPEN4_RESULT_LOCKTYPE_POSIX,
                                delegation: OpenDelegationType::None,
                                // Attrs actually applied — not an echo of the
                                // request mask (RFC 8881 §18.16.3 attrset).
                                attrset: attr_numbers_to_bitmap(&applied_attrs),
                            };
                        }
                        Err(e) => {
                            warn!("OPEN: Failed to generate filehandle for new file: {}", e);
                            return OpenRes {
                                status: Nfs4Status::Io,
                                stateid: None,
                                change_info: None,
                                result_flags: 0,
                                delegation: OpenDelegationType::None,
                                attrset: vec![],
                            };
                        }
                    }
                }
                Err(e) => {
                    warn!("OPEN: Failed to create file {:?}: {}", file_path, e);
                    let status = match e.kind() {
                        std::io::ErrorKind::PermissionDenied => Nfs4Status::Access,
                        std::io::ErrorKind::AlreadyExists => Nfs4Status::Exist,
                        std::io::ErrorKind::NotFound => Nfs4Status::NoEnt,
                        _ => Nfs4Status::Io,
                    };
                    return OpenRes {
                        status,
                        stateid: None,
                        change_info: None,
                        result_flags: 0,
                        delegation: OpenDelegationType::None,
                        attrset: vec![],
                    };
                }
            }
        }

        // OPEN without CREATE or CLAIM_FH - file must exist
        debug!("OPEN: Opening existing file (no create)");

        // Get client ID from session (set by SEQUENCE operation)
        let client_id = self.get_client_id_from_context(ctx);

        // Resolve to the FILE's filehandle for use as the
        // (client, owner, fh) key in `open_states`. For CLAIM_NULL
        // with no-create, `current_fh` is the parent directory; we
        // join the filename and look up the file's fh. For CLAIM_FH,
        // `current_fh` is already the file.
        //
        // F69: CFH MUST become the opened file (RFC 8881 §18.16.3).
        // Leaving it at the directory made the trailing GETATTR in the
        // client's atomic-open compound return the DIRECTORY's attrs;
        // the Linux client sees type=DIR, bails with EISDIR, re-opens
        // by handle, gets a bumped stateid seqid on a state it already
        // discarded, and parks in a 5-second schedule_timeout — the
        // fleet-wide cold-open stall.
        let parent_fh_data = current_fh.data.clone();
        // Also carry the target's PATH (and whether the fh resolved
        // live) so the open can be fd-anchored below (F17c). For a
        // CLAIM_FH whose object was renamed-over, the embedded path is
        // still parseable but must not be fresh-opened — only an
        // existing fd of the original inode may be reused.
        let (target_fh_data, target_path, target_live, target_full_fh): (
            Vec<u8>,
            Option<PathBuf>,
            bool,
            Option<Nfs4FileHandle>,
        ) = match &op.claim {
            OpenClaim::Null(name) => {
                let parent_path = self.fh_mgr.resolve_handle(current_fh).ok();
                if let Some(pp) = parent_path {
                    let file_path = pp.join(name);
                    match self.fh_mgr.path_to_filehandle(&file_path) {
                        Ok(fh) => (fh.data.clone(), Some(file_path), true, Some(fh)),
                        Err(_) => (parent_fh_data.clone(), Some(file_path), true, None),
                    }
                } else {
                    (parent_fh_data.clone(), None, false, None)
                }
            }
            OpenClaim::Fh => match self.fh_mgr.resolve_handle(current_fh) {
                Ok(p) => (parent_fh_data.clone(), Some(p), true, None),
                Err(_) => (
                    parent_fh_data.clone(),
                    FileHandleManager::parse_path_lenient(current_fh).ok(),
                    false,
                    None,
                ),
            },
        };

        // If opening for WRITE, recall any read delegations
        // share_access: 1 = READ, 2 = WRITE, 3 = BOTH
        if op.share_access & 2 != 0 {
            // Opening for write - recall read delegations
            if let Ok(file_path) = self.fh_mgr.resolve_handle(current_fh) {
                let recalled = self.state_mgr.delegations.recall_read_delegations(&file_path);
                if !recalled.is_empty() {
                    info!("📢 OPEN: Recalled {} read delegations for write access to {:?}",
                          recalled.len(), file_path);
                    // In a full implementation, we would wait for clients to return delegations
                    // For now, we just mark them as recalled and proceed
                }
            }
        }

        // RFC 8881 §9.7 cross-owner share-deny conflict. Courtesy-
        // cleanup ran at the top of dispatch_compound, so any
        // expired client's open-state has already been swept;
        // surviving conflicts are real.
        if self.state_mgr.stateids.share_conflict(
            &target_fh_data,
            client_id,
            &op.owner,
            op.share_access,
            op.share_deny,
        ) {
            warn!("OPEN(no-create): share-deny conflict → SHARE_DENIED");
            return OpenRes {
                status: Nfs4Status::ShareDenied,
                stateid: None,
                change_info: None,
                result_flags: 0,
                delegation: OpenDelegationType::None,
                attrset: vec![],
            };
        }

        // Record-or-bump the open (RFC 7530 §16.16: same (client,
        // owner, fh) gets the SAME stateid.other with seqid bumped,
        // share-mask merged).
        let stateid = self.state_mgr.stateids.record_open(
            client_id,
            op.owner.clone(),
            target_fh_data,
            op.share_access,
            op.share_deny,
            None,
        );

        debug!("OPEN: stateid {:?} for client {}", stateid, client_id);

        // F17c: anchor the open with an fd immediately.
        if let Some(p) = &target_path {
            self.seed_open_fd(&stateid, p, target_live);
        }

        // Try to grant read delegation if appropriate
        let delegation = self.try_grant_read_delegation(
            client_id,
            current_fh,
            op.share_access,
        );

        // F69: CFH becomes the opened file (RFC 8881 §18.16.3) so the
        // GETFH/GETATTR the client appends to its OPEN compound
        // describe the file, not the parent directory. CLAIM_FH opens
        // arrive with the file already as CFH (target_full_fh=None).
        if let Some(fh) = target_full_fh {
            ctx.set_current_fh(fh);
        }

        OpenRes {
            status: Nfs4Status::Ok,
            stateid: Some(stateid),
            change_info: Some(ChangeInfo {
                atomic: true,
                before: 0,
                after: 1,
            }),
            result_flags: OPEN4_RESULT_LOCKTYPE_POSIX,
            delegation,
            attrset: vec![],
        }
    }

    /// Try to grant a read delegation
    ///
    /// Read delegations can be granted if:
    /// - Client is opening for READ only (not WRITE)
    /// - No other clients have the file open for WRITE
    /// - File is not being actively modified
    fn try_grant_read_delegation(
        &self,
        client_id: u64,
        filehandle: &Nfs4FileHandle,
        share_access: u32,
    ) -> OpenDelegationType {
        // Delegations are disabled unless FLINT_NFS_DELEGATIONS=1: granting
        // one is only safe with a working recall path (CB_NULL probe +
        // CB_RECALL + block-conflicting-opens), which doesn't exist yet — a
        // client holding an unrecallable read delegation may serve stale
        // cache forever after another client writes. Never granting is
        // fully RFC-compliant (RFC 8881 §10.4). Note the OPEN encoder
        // currently hardcodes OPEN_DELEGATE_NONE anyway; this gate keeps
        // the server from minting phantom delegation records the client
        // never hears about, and keeps the trap disarmed if the encoder is
        // ever made honest.
        if !delegations_enabled() {
            return OpenDelegationType::None;
        }

        // Only grant read delegations for READ-only opens
        // share_access: 1 = READ, 2 = WRITE, 3 = BOTH
        if share_access != 1 {
            debug!("OPEN: Not granting delegation - not read-only access");
            return OpenDelegationType::None;
        }

        // Resolve file path
        let file_path = match self.fh_mgr.resolve_handle(filehandle) {
            Ok(path) => path,
            Err(e) => {
                debug!("OPEN: Cannot grant delegation - failed to resolve path: {}", e);
                return OpenDelegationType::None;
            }
        };

        // Try to grant read delegation
        match self.state_mgr.delegations.grant_read_delegation(
            client_id,
            filehandle.data.clone(),
            file_path,
        ) {
            Some(deleg_stateid) => {
                info!("✅ OPEN: Granted read delegation {:?} to client {}", deleg_stateid, client_id);
                OpenDelegationType::Read
            }
            None => {
                debug!("OPEN: Cannot grant delegation - conflicts exist");
                OpenDelegationType::None
            }
        }
    }

    /// Handle DELEGRETURN operation
    pub fn handle_delegreturn(
        &self,
        stateid: StateId,
        _ctx: &CompoundContext,
    ) -> DelegReturnRes {
        debug!("DELEGRETURN: stateid={:?}", stateid);

        // Return the delegation
        match self.state_mgr.delegations.return_delegation(&stateid) {
            Ok(()) => {
                info!("✅ DELEGRETURN: Successfully returned delegation {:?}", stateid);
                DelegReturnRes {
                    status: Nfs4Status::Ok,
                }
            }
            Err(status) => {
                warn!("❌ DELEGRETURN: Failed to return delegation {:?}: {:?}", stateid, status);
                DelegReturnRes {
                    status,
                }
            }
        }
    }

    /// Handle CLOSE operation
    pub fn handle_close(
        &self,
        op: CloseOp,
        _ctx: &CompoundContext,
    ) -> CloseRes {
        debug!("CLOSE: stateid={:?}", op.stateid);

        // F31: atomic seqid-checked close. The outcome discrimination is
        // load-bearing: OLD_STATEID tells the client "your view is stale,
        // refresh" (benign — it re-evaluates whether it still wants the
        // close), while BAD_STATEID detonates a TEST_STATEID recovery
        // round that stalls the whole session. A reordered CLOSE racing
        // a same-owner re-OPEN must take the former path, and must not
        // destroy the state the new opener holds.
        use crate::nfs::v4::state::stateid::CloseOutcome;
        match self.state_mgr.stateids.close_open(&op.stateid) {
            CloseOutcome::Closed => {
                // Remove file descriptor from cache (file closes on drop)
                // — only now that the state is truly gone; a refused
                // close must keep the fd anchored (F17c).
                if let Some(cached) = self.fd_cache.remove(&op.stateid.other) {
                    debug!(
                        "🗑️ FD CACHE CLOSE: Removed and closed FD for {:?} (path: {:?})",
                        op.stateid, cached.path
                    );
                }
                debug!("CLOSE: Removed open state for {:?}", op.stateid);
                CloseRes {
                    status: Nfs4Status::Ok,
                    stateid: Some(StateId {
                        seqid: op.stateid.seqid + 1,
                        other: op.stateid.other,
                    }),
                }
            }
            CloseOutcome::OldStateId => {
                debug!(
                    "CLOSE: stale seqid or recently-closed replay (other={:02x?} seqid={}) → OLD_STATEID",
                    op.stateid.other, op.stateid.seqid
                );
                CloseRes {
                    status: Nfs4Status::OldStateId,
                    stateid: None,
                }
            }
            outcome => {
                // F28: stateid identity in the warn — correlating failing
                // CLOSEs against OPEN grants distinguishes lease-reap vs
                // replay-miss vs fh-keying as a storm's seed.
                warn!(
                    "CLOSE: Invalid stateid: {:?} (other={:02x?} seqid={})",
                    outcome, op.stateid.other, op.stateid.seqid
                );
                CloseRes {
                    status: Nfs4Status::BadStateId,
                    stateid: None,
                }
            }
        }
    }

    /// Handle READ operation
    pub async fn handle_read(
        &self,
        op: ReadOp,
        ctx: &CompoundContext,
    ) -> ReadRes {
        debug!("READ: stateid={:?}, offset={}, count={}",
               op.stateid, op.offset, op.count);

        // Check current filehandle
        let current_fh = match &ctx.current_fh {
            Some(fh) => fh,
            None => {
                return ReadRes {
                    status: Nfs4Status::NoFileHandle,
                    eof: false,
                    data: Bytes::new(),
                };
            }
        };

        // Validate stateid with relaxed checking for READ operations
        // This allows seqid=0 for anonymous/first reads
        if let Err(e) = self.state_mgr.stateids.validate_for_read(&op.stateid) {
            warn!("READ: Invalid stateid: {}", e);
            return ReadRes {
                status: Nfs4Status::BadStateId,
                eof: false,
                data: Bytes::new(),
            };
        }

        // Resolve file path from filehandle. A stale resolve (object
        // renamed-over/removed) still serves through an open fd — the
        // original file is alive under POSIX unlink-open semantics
        // (F17b); only when no open exists is STALE the answer.
        let mut stale_fd: Option<Arc<File>> = None;
        let path = match self.fh_mgr.resolve_handle(current_fh) {
            Ok(p) => p,
            Err(e) => match self.stale_open_fallback(current_fh, &op.stateid.other, false) {
                Some((p, f)) => {
                    debug!("READ: {:?} replaced on disk; serving via open fd", p);
                    stale_fd = Some(f);
                    p
                }
                None => {
                    warn!("READ: Failed to resolve file handle: {}", e);
                    return ReadRes {
                        status: Nfs4Status::Stale,
                        eof: false,
                        data: Bytes::new(),
                    };
                }
            },
        };

        // TEST-ONLY: hold READs of a ".cold." file until its fake
        // hydration completes, to measure the client kernel's tolerance
        // of hydration latency (see hydration_stall_deadline below;
        // inert unless the env var is set).
        if let Some(stall) = hydration_stall_secs() {
            if path.to_string_lossy().contains(".cold.") {
                if let Some(dl) = hydration_stall_deadline(&path, stall) {
                    warn!(
                        "TEST hydration stall: holding READ of {:?} (offset {})",
                        path, op.offset
                    );
                    tokio::time::sleep_until(tokio::time::Instant::from_std(dl)).await;
                }
            }
        }

        // TEST-ONLY (step 9's rig gate): answer READs of a ".cold."
        // file with NFS4ERR_DELAY until its fake hydration completes —
        // the A5 DELAY-parking posture (slot released immediately),
        // opposite of the in-RPC hold above. Every answer logs the
        // per-file attempt number and elapsed time, so the server log
        // IS the client's retry-cadence record. Inert unless set.
        if let Some(secs) = hydration_delay_secs() {
            if path.to_string_lossy().contains(".cold.") {
                if let Some((n, since)) = hydration_delay_pending(&path, secs) {
                    warn!(
                        "TEST hydration DELAY: READ attempt {} of {:?} at +{:.3}s \
                         (offset {}) → NFS4ERR_DELAY",
                        n, path, since, op.offset
                    );
                    return ReadRes { status: Nfs4Status::Delay, eof: false, data: Bytes::new() };
                }
            }
        }

        // Get filename for logging before moving path
        let filename = path.file_name().map(|n| n.to_string_lossy().to_string());

        // Reuse the cached fd for this stateid when it maps to the
        // same file; otherwise open and cache. The path check guards
        // against a stateid presented with a different filehandle.
        let cacheable = cacheable_stateid(&op.stateid.other);
        let cached = stale_fd.or_else(|| {
            if cacheable {
                self.fd_cache
                    .get(&op.stateid.other)
                    .filter(|e| e.path == path)
                    .map(|e| Arc::clone(&e.file))
            } else {
                None
            }
        });

        // Perform positioned read using blocking I/O
        // Uses positioned I/O (pread) for concurrent access without seek
        let offset = op.offset;
        let count = op.count as usize;
        let fd_cache = Arc::clone(&self.fd_cache);
        let stateid_other = op.stateid.other;

        let read_result = tokio::task::spawn_blocking(move || -> std::io::Result<(Bytes, bool)> {
            let file = match cached {
                Some(f) => f,
                None => {
                    // Prefer read+write so a later WRITE on this
                    // stateid reuses the entry; fall back to
                    // read-only when the file mode denies write.
                    let (file, writable) = match std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(&path)
                    {
                        Ok(f) => (f, true),
                        Err(_) => (std::fs::File::open(&path)?, false),
                    };
                    let file = Arc::new(file);
                    if cacheable {
                        fd_cache.insert(stateid_other, CachedFile {
                            file: Arc::clone(&file),
                            path: path.clone(),
                            writable,
                            ino: CachedFile::ino_of(&file),
                        });
                    }
                    file
                }
            };

            // Get file size to determine EOF
            let metadata = file.metadata()?;
            // Step 10 (C2): consult the eviction marker BEFORE trusting
            // local size — an evicted file is a 0-byte stub whose bytes
            // live in the bucket; its size-based eof would read as
            // empty. DELAY parks the reader (measured GO, step 9);
            // step 11 turns this into hydrate-then-serve. Residual
            // cached fds are safe for exactly this reason: every op
            // re-checks.
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if crate::tier::evict::is_evicted(metadata.dev(), metadata.ino()) {
                    crate::tier::meter::bump(crate::tier::meter::Counter::EvictedOpDelays);
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WouldBlock,
                        "tier: file evicted (awaiting hydration)",
                    ));
                }
            }
            let file_size = metadata.len();

            // Determine actual read count (don't read past EOF)
            let actual_count = if offset >= file_size {
                0
            } else {
                std::cmp::min(count, (file_size - offset) as usize)
            };
            
            if actual_count == 0 {
                return Ok((Bytes::new(), true));
            }

            // Read data using positioned I/O (no seek needed - concurrent safe!)
            let mut buffer = vec![0u8; actual_count];
            let bytes_read = file.read_at(&mut buffer, offset)?;
            
            buffer.truncate(bytes_read);
            let eof = offset + bytes_read as u64 >= file_size;
            
            Ok((Bytes::from(buffer), eof))
        }).await;

        match read_result {
            Ok(Ok((data, eof))) => {
                debug!("READ: Read {} bytes at offset {} from {:?}, eof={}",
                      data.len(), op.offset, filename.as_deref().unwrap_or("unknown"), eof);
                ReadRes {
                    status: Nfs4Status::Ok,
                    eof,
                    data,
                }
            }
            Ok(Err(e)) => {
                warn!("READ: I/O error reading file: {}", e);
                let status = match e.kind() {
                    std::io::ErrorKind::NotFound => Nfs4Status::NoEnt,
                    std::io::ErrorKind::PermissionDenied => Nfs4Status::Access,
                    std::io::ErrorKind::IsADirectory => Nfs4Status::IsDir,
                    // Step 10: evicted file — the client retries until
                    // hydration (step 11) restores the bytes.
                    std::io::ErrorKind::WouldBlock => Nfs4Status::Delay,
                    _ => Nfs4Status::Io,
                };
                ReadRes {
                    status,
                    eof: false,
                    data: Bytes::new(),
                }
            }
            Err(e) => {
                warn!("READ: Task spawn error: {}", e);
                ReadRes {
                    status: Nfs4Status::Io,
                    eof: false,
                    data: Bytes::new(),
                }
            }
        }
    }

    /// Handle WRITE operation
    pub async fn handle_write(
        &self,
        op: WriteOp,
        ctx: &CompoundContext,
    ) -> WriteRes {
        debug!("WRITE: stateid={:?}, offset={}, count={}, stable={}",
               op.stateid, op.offset, op.data.len(), op.stable);

        // Check current filehandle
        let current_fh = match &ctx.current_fh {
            Some(fh) => fh,
            None => {
                return WriteRes {
                    status: Nfs4Status::NoFileHandle,
                    count: 0,
                    committed: UNSTABLE4,
                    writeverf: 0,
                };
            }
        };

        // RFC 5661 §18.32: WRITE requires a stateid with an exact seqid. The
        // `ANONYMOUS_STATEID` is allowed only for opens that don't establish
        // share state, and `validate()` short-circuits on it. The previous
        // form used the relaxed READ validator, which accepted any seqid=0
        // stateid as anonymous — that's a write-share-deny bypass.
        if let Err(e) = self.state_mgr.stateids.validate(&op.stateid) {
            warn!("WRITE: Invalid stateid: {}", e);
            return WriteRes {
                status: Nfs4Status::BadStateId,
                count: 0,
                committed: UNSTABLE4,
                writeverf: 0,
            };
        }

        // Resolve file path from filehandle. Stale resolve → serve
        // through an open fd when one exists (F17b, see handle_read) —
        // writeback for a renamed-over file must land in the ORIGINAL
        // inode, never fail the client's flush.
        let mut stale_fd: Option<Arc<File>> = None;
        let path = match self.fh_mgr.resolve_handle(current_fh) {
            Ok(p) => p,
            Err(e) => match self.stale_open_fallback(current_fh, &op.stateid.other, true) {
                Some((p, f)) => {
                    debug!("WRITE: {:?} replaced on disk; writing via open fd", p);
                    stale_fd = Some(f);
                    p
                }
                None => {
                    warn!("WRITE: Failed to resolve file handle: {}", e);
                    return WriteRes {
                        status: Nfs4Status::Stale,
                        count: 0,
                        committed: UNSTABLE4,
                        writeverf: 0,
                    };
                }
            },
        };

        // Get filename for logging before moving path
        let filename = path.file_name().map(|n| n.to_string_lossy().to_string());

        // Try to get cached file descriptor first. Only writable
        // entries for the same file qualify — READ may have cached a
        // read-only fd, and a stateid presented with a different
        // filehandle must not reuse another file's fd.
        let cacheable = cacheable_stateid(&op.stateid.other);
        let cached_entry = stale_fd.or_else(|| {
            if cacheable {
                self.fd_cache
                    .get(&op.stateid.other)
                    .filter(|e| e.writable && e.path == path)
                    .map(|e| Arc::clone(&e.file))
            } else {
                None
            }
        });

        let file_arc = if let Some(file) = cached_entry {
            // Found in cache - reuse existing FD!
            debug!("✅ FD CACHE HIT: Reusing cached file descriptor for {:?}", op.stateid);
            file
        } else {
            // Not in cache - open and cache it
            debug!("🔧 FD CACHE MISS: Opening file and caching for {:?} (path: {:?})", op.stateid, path);
            
            let path_clone = path.clone();
            let file_result = tokio::task::spawn_blocking(move || {
                std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .open(&path_clone)
            }).await;
            
            let file_arc: Arc<File> = match file_result {
                Ok(Ok(f)) => {
                    let file_arc = Arc::new(f);
                    // Cache the file descriptor (never under special stateids)
                    if cacheable {
                        self.fd_cache.insert(op.stateid.other, CachedFile {
                            file: Arc::clone(&file_arc),
                            path: path.clone(),
                            writable: true,
                            ino: CachedFile::ino_of(&file_arc),
                        });
                        debug!("WRITE: Cached new FD for {:?} (path: {:?})", op.stateid, path);
                    }
                    file_arc
                }
                Ok(Err(e)) => {
                    // POSIX checks permission at OPEN time, not per
                    // write: a file chmoded read-only AFTER a valid
                    // write-open must keep accepting that open's
                    // flushes. git fchmods every loose object to 0444
                    // before close, and small writes flush LAST — so
                    // the first WRITE RPC can arrive after the chmod
                    // and a fresh open here gets EACCES. The open-time
                    // fd is in the cache under some key (F17c seeds it
                    // at OPEN); the stateid-keyed lookup above can
                    // still miss it on a path-form mismatch or an
                    // eviction, so find it BY INODE before surfacing
                    // EIO to a legally-open writer.
                    let by_ino_fd = std::fs::metadata(&path)
                        .ok()
                        .map(|m| {
                            use std::os::unix::fs::MetadataExt;
                            m.ino()
                        })
                        .and_then(|ino| self.fd_cache.find_by_ino(ino, true));
                    match by_ino_fd {
                        Some(hit) => {
                            debug!(
                                "WRITE: open failed ({}) but a writable open-time fd \
                                 exists for the same inode — serving the flush from it",
                                e
                            );
                            if cacheable {
                                self.fd_cache.insert(
                                    op.stateid.other,
                                    CachedFile {
                                        file: Arc::clone(&hit.file),
                                        path: path.clone(),
                                        writable: true,
                                        ino: hit.ino,
                                    },
                                );
                            }
                            hit.file
                        }
                        None => {
                            warn!("WRITE: Failed to open file {:?}: {}", path, e);
                            return WriteRes {
                                status: Nfs4Status::Io,
                                count: 0,
                                committed: UNSTABLE4,
                                writeverf: 0,
                            };
                        }
                    }
                }
                Err(e) => {
                    warn!("WRITE: spawn_blocking error: {}", e);
                    return WriteRes {
                        status: Nfs4Status::Io,
                        count: 0,
                        committed: UNSTABLE4,
                        writeverf: 0,
                    };
                }
            };

            file_arc
        };

        // Perform positioned write using cached/opened file
        // ZERO-COPY: data is Bytes (Arc-backed), clone is cheap
        let offset = op.offset;
        let data_clone = op.data.clone(); // Cheap: just Arc increment
        let stable = op.stable;
        let write_verifier = self.write_verifier;
        // A2 capture: the durable mark wants the path. Cloned only when
        // capture is on — this is the hottest lane in the server.
        let cap_path = crate::tier::capture::enabled().then(|| path.clone());

        // A10 admission: NOSPC delivered BEFORE hard-full, while the
        // reserve still holds — the errno applications handle, and the
        // headroom the flusher/state.db need stays theirs. One relaxed
        // load when the tier is off.
        if crate::tier::space::admit_bytes(&path, op.data.len() as u64).is_err() {
            warn!("WRITE: refused NOSPC — PVC headroom-minus-reserve exhausted");
            return WriteRes {
                status: Nfs4Status::NoSpc,
                count: 0,
                committed: UNSTABLE4,
                writeverf: 0,
            };
        }

        // TEST-ONLY (step 9's rig gate): WRITEs of a ".cold." file
        // answer NFS4ERR_DELAY while its fake hydration is pending —
        // the hydrate-first WRITE barrier's wire shape (A5). Measures
        // the kernel's writeback retry cadence. Inert unless set.
        if let Some(secs) = hydration_delay_secs() {
            if path.to_string_lossy().contains(".cold.") {
                if let Some((n, since)) = hydration_delay_pending(&path, secs) {
                    warn!(
                        "TEST hydration DELAY: WRITE attempt {} of {:?} at +{:.3}s \
                         (offset {}) → NFS4ERR_DELAY",
                        n, path, since, op.offset
                    );
                    return WriteRes {
                        status: Nfs4Status::Delay,
                        count: 0,
                        committed: UNSTABLE4,
                        writeverf: 0,
                    };
                }
            }
        }

        let write_result = tokio::task::spawn_blocking(move || -> std::io::Result<usize> {
            use std::os::unix::fs::FileExt;

            // A4 write gate: held across the pwrite AND the capture
            // note below, so a gate drain never swaps an epoch between
            // them and an excluded (evicting/hydrating) file refuses
            // the write before any byte moves. No-op when the tier is
            // off. WouldBlock maps to NFS4ERR_DELAY in the caller.
            let _gate = crate::tier::gate::enter_file(&file_arc).map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "tier: file excluded (evicting/hydrating)",
                )
            })?;

            // Step 10 (C6): a WRITE to an EVICTED file must never land
            // bytes in the stub — the next flush's part-rounding would
            // publish sparse zeros over generation data. DELAY until
            // step 11 makes writes hydrate-first. (The gate's exclusion
            // covers the eviction WINDOW; this marker check covers the
            // evicted steady state after the exclusion dropped.)
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if let Ok(md) = file_arc.metadata() {
                    if crate::tier::evict::is_evicted(md.dev(), md.ino()) {
                        crate::tier::meter::bump(
                            crate::tier::meter::Counter::EvictedOpDelays,
                        );
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::WouldBlock,
                            "tier: file evicted (awaiting hydration)",
                        ));
                    }
                }
            }

            // Positioned I/O: pwrite(2) takes an offset and is safe to
            // call concurrently from multiple threads on the same
            // file descriptor (no seek pointer is mutated). No mutex
            // needed; the kernel serializes at the page-cache level.
            let bytes_written = file_arc.write_at(&data_clone, offset)?;

            // Handle stability requirement
            // UNSTABLE4 (0): Can cache, flush later (fast)
            // DATA_SYNC4 (1): Sync data, metadata can be cached
            // FILE_SYNC4 (2): Sync both data and metadata (slow)
            if stable == FILE_SYNC4 {
                file_arc.sync_all()?; // Full fsync
            } else if stable == DATA_SYNC4 {
                file_arc.sync_data()?; // Sync data only
            }
            // UNSTABLE4: no sync, will be done on COMMIT

            // F14: advance the file's monotonic change counter — the
            // GETATTR appended to this WRITE compound must carry a value
            // strictly newer than any pre-write GETATTR, even inside one
            // filesystem clock tick.
            if let Ok(md) = file_arc.metadata() {
                use std::os::unix::fs::MetadataExt;
                crate::nfs::v4::change_counter::bump(
                    md.dev(),
                    md.ino(),
                    crate::nfs::v4::change_counter::ctime_ns(&md),
                );
                // A2 dirty capture rides the same post-success point:
                // a content bump without a note is the C5 bug class.
                crate::tier::capture::note_at(
                    md.dev(),
                    md.ino(),
                    cap_path.as_deref(),
                    crate::tier::capture::Mutation::Write {
                        offset,
                        len: bytes_written as u64,
                    },
                );
            }

            Ok(bytes_written)
        }).await;

        match write_result {
            Ok(Ok(bytes_written)) => {
                let count = bytes_written as u32;
                debug!("WRITE: Wrote {} bytes at offset {} to {:?}, stable={}",
                      count, offset, filename.as_deref().unwrap_or("unknown"), stable);
                WriteRes {
                    status: Nfs4Status::Ok,
                    count,
                    committed: stable,
                    writeverf: write_verifier,
                }
            }
            Ok(Err(e)) => {
                warn!("WRITE: I/O error writing file: {}", e);
                // A10: errno first — ENOSPC/EDQUOT must not collapse
                // into EIO (the mapping the DS lane has carried since
                // the ENOSPC drill).
                let status = super::errno_status(&e).unwrap_or(match e.kind() {
                    std::io::ErrorKind::NotFound => Nfs4Status::NoEnt,
                    std::io::ErrorKind::PermissionDenied => Nfs4Status::Access,
                    std::io::ErrorKind::IsADirectory => Nfs4Status::IsDir,
                    // A4 gate refusal: the file is mid-evict/hydrate;
                    // the client retries after a short delay.
                    std::io::ErrorKind::WouldBlock => Nfs4Status::Delay,
                    _ => Nfs4Status::Io,
                });
                WriteRes {
                    status,
                    count: 0,
                    committed: UNSTABLE4,
                    writeverf: 0,
                }
            }
            Err(e) => {
                warn!("WRITE: Task spawn error: {}", e);
                WriteRes {
                    status: Nfs4Status::Io,
                    count: 0,
                    committed: UNSTABLE4,
                    writeverf: 0,
                }
            }
        }
    }

    /// Handle COMMIT operation
    pub async fn handle_commit(
        &self,
        op: CommitOp,
        ctx: &CompoundContext,
    ) -> CommitRes {
        debug!("COMMIT: offset={}, count={}", op.offset, op.count);

        // Check current filehandle
        let current_fh = match &ctx.current_fh {
            Some(fh) => fh,
            None => {
                return CommitRes {
                    status: Nfs4Status::NoFileHandle,
                    writeverf: 0,
                };
            }
        };

        // Resolve file path from filehandle
        let path = match self.fh_mgr.resolve_handle(current_fh) {
            Ok(p) => p,
            Err(e) => {
                warn!("COMMIT: Failed to resolve file handle: {}", e);
                return CommitRes {
                    status: Nfs4Status::Stale,
                    writeverf: 0,
                };
            }
        };

        // Get filename for logging before moving path
        let filename = path.file_name().map(|n| n.to_string_lossy().to_string());

        // Perform fsync to commit UNSTABLE writes to stable storage
        // This is critical for data integrity!
        let write_verifier = self.write_verifier;

        // Try to reuse the WRITE-side cached fd: COMMIT carries no
        // stateid (RFC 5661 §18.3 — `count4` and `offset4` only), so
        // we look up any cached fd whose path matches. This avoids a
        // namespace lookup + open syscall on every fsync, which on
        // fsync-heavy workloads is measurable. Falls back to the
        // open-fresh path if no cached fd exists (e.g. the file was
        // committed by a different connection or the cache evicted).
        let cached_fd = self.fd_cache.find_by_path(&path, false).map(|e| e.file);

        let commit_result = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            if let Some(file_arc) = cached_fd {
                // Hot path: reuse the WRITE-side cached fd. sync_all
                // takes &File so the Arc<File> works directly — no
                // mutex needed (positioned I/O is concurrency-safe).
                file_arc.sync_all()?;
                Ok(())
            } else {
                // Cold path: no cached fd. Open fresh and sync.
                let file = std::fs::OpenOptions::new()
                    .write(true)
                    .open(&path)?;
                file.sync_all()?;
                Ok(())
            }
        }).await;

        match commit_result {
            Ok(Ok(())) => {
                debug!("COMMIT: Synced data to disk for {:?}", filename.as_deref().unwrap_or("unknown"));
                CommitRes {
                    status: Nfs4Status::Ok,
                    writeverf: write_verifier,
                }
            }
            Ok(Err(e)) => {
                warn!("COMMIT: I/O error syncing file: {}", e);
                // A10: fsync is where delayed-allocation ENOSPC lands —
                // exactly the reply that must NOT read as EIO.
                let status = super::errno_status(&e).unwrap_or(match e.kind() {
                    std::io::ErrorKind::NotFound => Nfs4Status::NoEnt,
                    std::io::ErrorKind::PermissionDenied => Nfs4Status::Access,
                    _ => Nfs4Status::Io,
                });
                CommitRes {
                    status,
                    writeverf: 0,
                }
            }
            Err(e) => {
                warn!("COMMIT: Task spawn error: {}", e);
                CommitRes {
                    status: Nfs4Status::Io,
                    writeverf: 0,
                }
            }
        }
    }
}

/// TEST-ONLY hydration-stall injector (S3 cold-tier gate (b), the
/// client-kernel READ-hold question): with
/// `FLINT_TEST_HYDRATION_STALL_SECS=N`, the FIRST READ of any file
/// whose path contains ".cold." is held N seconds before serving —
/// the exact shape of a whole-file hydration from S3 blocking the
/// first reader. Unset (the default) this is one None-check per READ.
fn hydration_stall_secs() -> Option<u64> {
    static V: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    *V.get_or_init(|| {
        std::env::var("FLINT_TEST_HYDRATION_STALL_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|n| *n > 0)
    })
}

/// The injector's "hydration in progress" table. The FIRST READ of a
/// cold path starts a fake hydration ending `stall` seconds later;
/// EVERY READ of that path (readahead fans out many in parallel)
/// returns the deadline while it is still in the future — real
/// hydration blocks all readers, not just the first RPC, and the
/// session-slot question depends on exactly that.
fn hydration_stall_deadline(
    path: &std::path::Path,
    stall: u64,
) -> Option<std::time::Instant> {
    static MAP: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, std::time::Instant>>,
    > = std::sync::OnceLock::new();
    let mut m = MAP
        .get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
        .lock()
        .unwrap();
    let now = std::time::Instant::now();
    let dl = *m
        .entry(path.to_path_buf())
        .or_insert_with(|| now + std::time::Duration::from_secs(stall));
    (dl > now).then_some(dl)
}

/// TEST-ONLY DELAY-parking injector (step 9's rig gate, the A5
/// posture): with `FLINT_TEST_HYDRATION_DELAY_SECS=N`, the first
/// touch of a ".cold." file starts a fake N-second hydration, and
/// every READ/WRITE while it is pending is answered NFS4ERR_DELAY —
/// the session slot is released immediately, unlike the in-RPC hold
/// above. Unset (the default) this is one None-check per op.
fn hydration_delay_secs() -> Option<u64> {
    static V: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    *V.get_or_init(|| {
        std::env::var("FLINT_TEST_HYDRATION_DELAY_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|n| *n > 0)
    })
}

/// Returns `Some((attempt_no, secs_since_first_touch))` while the fake
/// hydration is pending — the caller answers DELAY and logs both, so
/// the server log records the client's exact retry cadence. `None`
/// once the deadline passed: serve normally.
fn hydration_delay_pending(path: &std::path::Path, secs: u64) -> Option<(u64, f64)> {
    struct Fake {
        start: std::time::Instant,
        deadline: std::time::Instant,
        attempts: u64,
    }
    static MAP: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, Fake>>,
    > = std::sync::OnceLock::new();
    let mut m = MAP
        .get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
        .lock()
        .unwrap();
    let now = std::time::Instant::now();
    let f = m.entry(path.to_path_buf()).or_insert_with(|| Fake {
        start: now,
        deadline: now + std::time::Duration::from_secs(secs),
        attempts: 0,
    });
    if f.deadline > now {
        f.attempts += 1;
        Some((f.attempts, now.duration_since(f.start).as_secs_f64()))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nfs::v4::filehandle::FileHandleManager;
    use crate::nfs::v4::state::StateType;
    use tempfile::TempDir;

    /// A2 census (design review C5): a WRITE through the real handler
    /// must land in the tier capture log with its exact byte range.
    #[tokio::test]
    async fn write_notes_tier_capture() {
        use std::os::unix::fs::MetadataExt;
        crate::tier::capture::force_enable();
        let (handler, fh_mgr, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);
        let export_fh = fh_mgr.path_to_filehandle(fh_mgr.get_export_path()).unwrap();
        ctx.current_fh = Some(export_fh);
        let open_res = handler
            .handle_open(
                OpenOp {
                    seqid: 0,
                    share_access: OPEN4_SHARE_ACCESS_BOTH,
                    share_deny: OPEN4_SHARE_DENY_NONE,
                    owner: b"census-owner".to_vec(),
                    openhow: OpenHow::Create(Fattr4 { attrmask: vec![], attr_vals: vec![] }),
                    claim: OpenClaim::Null("census-write.bin".to_string()),
                },
                &mut ctx,
            )
            .await;
        assert_eq!(open_res.status, Nfs4Status::Ok);
        let stateid = open_res.stateid.unwrap();
        // ext4 REUSES inode numbers: a dead test file's capture residue
        // can alias onto this identity (safe in production — pessimal
        // upload — but this test asserts EXACT capture state). Clear it.
        {
            let m = std::fs::metadata(fh_mgr.get_export_path().join("census-write.bin")).unwrap();
            crate::tier::capture::forget(m.dev(), m.ino());
        }

        let res = handler
            .handle_write(
                WriteOp {
                    stateid,
                    offset: 4096,
                    stable: FILE_SYNC4,
                    data: Bytes::from(vec![7u8; 100]),
                },
                &ctx,
            )
            .await;
        assert_eq!(res.status, Nfs4Status::Ok);

        let md = std::fs::metadata(fh_mgr.get_export_path().join("census-write.bin")).unwrap();
        let cap = crate::tier::capture::snapshot(md.dev(), md.ino())
            .expect("WRITE must note the tier capture (C5: a bump without a note)");
        assert_eq!(cap.intervals, vec![(4096, 4196)]);
        assert!(!cap.whole);
    }

    /// A4 write gate at the WRITE site: an excluded (evicting/
    /// hydrating) file refuses with NFS4ERR_DELAY before any byte
    /// moves, notes nothing, and serves normally once the exclusion
    /// drops.
    #[tokio::test]
    async fn write_refused_with_delay_while_excluded() {
        use std::os::unix::fs::MetadataExt;
        crate::tier::capture::force_enable();
        let (handler, fh_mgr, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);
        let export_fh = fh_mgr.path_to_filehandle(fh_mgr.get_export_path()).unwrap();
        ctx.current_fh = Some(export_fh);
        let open_res = handler
            .handle_open(
                OpenOp {
                    seqid: 0,
                    share_access: OPEN4_SHARE_ACCESS_BOTH,
                    share_deny: OPEN4_SHARE_DENY_NONE,
                    owner: b"gate-owner".to_vec(),
                    openhow: OpenHow::Create(Fattr4 { attrmask: vec![], attr_vals: vec![] }),
                    claim: OpenClaim::Null("gate-write.bin".to_string()),
                },
                &mut ctx,
            )
            .await;
        assert_eq!(open_res.status, Nfs4Status::Ok);
        let stateid = open_res.stateid.unwrap();
        let write = |off: u64| WriteOp {
            stateid: stateid.clone(),
            offset: off,
            stable: FILE_SYNC4,
            data: Bytes::from(vec![9u8; 8]),
        };

        let md = std::fs::metadata(fh_mgr.get_export_path().join("gate-write.bin")).unwrap();
        let (dev, ino) = (md.dev(), md.ino());
        // ext4 REUSES inode numbers: a dead test file's capture residue
        // can alias onto this identity (safe in production — pessimal
        // upload — but this test asserts EXACT capture state). Clear it.
        crate::tier::capture::forget(dev, ino);
        let excl = crate::tier::gate::exclude(dev, ino);
        let res = handler.handle_write(write(0), &ctx).await;
        assert_eq!(
            res.status,
            Nfs4Status::Delay,
            "an excluded file must refuse WRITE with DELAY"
        );
        assert_eq!(res.count, 0);
        assert!(
            crate::tier::capture::snapshot(dev, ino).is_none_or(|c| !c.is_dirty()),
            "a refused WRITE must note nothing"
        );

        drop(excl);
        let res = handler.handle_write(write(16), &ctx).await;
        assert_eq!(res.status, Nfs4Status::Ok, "WRITE must serve once the exclusion drops");
        let cap = crate::tier::capture::snapshot(dev, ino).expect("the retry must note");
        assert_eq!(cap.intervals, vec![(16, 24)]);
    }

    /// F17b: a renamed-over file keeps serving through its open fd —
    /// READ via the stale handle returns the ORIGINAL bytes, and the
    /// open-file view finds the fd by embedded path. Once no open fd
    /// exists, the stale handle answers Stale.
    #[tokio::test]
    async fn read_serves_renamed_over_file_via_open_fd() {
        let (handler, fh_mgr, temp) = create_test_handler();
        let target = temp.path().join("renamed.dat");
        std::fs::write(&target, b"generation-1 payload").unwrap();
        let fh = fh_mgr.path_to_filehandle(&target).unwrap();

        // Server-side open (as OPEN would cache it) BEFORE the rename.
        let file = Arc::new(std::fs::File::open(&target).unwrap());
        let other = [7u8; 12];
        handler.test_seed_fd(other, Arc::clone(&file), target.clone(), false);

        // rename-over: path now names a different inode.
        let tmp = temp.path().join("renamed.dat.tmp");
        std::fs::write(&tmp, b"generation-2 REPLACED").unwrap();
        std::fs::rename(&tmp, &target).unwrap();

        // Resolve is stale, but the fallback finds the open fd.
        assert!(fh_mgr.resolve_handle(&fh).is_err());
        let (p, f) = handler
            .stale_open_fallback(&fh, &other, false)
            .expect("open fd must be found for the stale handle");
        assert_eq!(p, target);
        use std::os::unix::fs::FileExt;
        let mut buf = [0u8; 12];
        f.read_exact_at(&mut buf, 0).unwrap();
        assert_eq!(&buf, b"generation-1", "must read the ORIGINAL file");

        // Writable-only lookup must not return the read-only fd.
        assert!(handler.stale_open_fallback(&fh, &other, true).is_none());

        // View by path works for fh-only ops (GETATTR).
        let view = handler.open_file_view();
        assert!(view.file_for_path(&target).is_some());

        // No open fd → honest STALE.
        handler.fd_cache.remove(&other);
        assert!(handler.stale_open_fallback(&fh, &other, false).is_none());
        assert!(view.file_for_path(&target).is_none());
    }

    /// Regression: seeding an fd for a path that already has a cached
    /// fd must not deadlock when the new stateid hashes to the shard
    /// the scan matched in. The buggy version held the DashMap Iter's
    /// shard read guard across the insert (if-let scrutinee temporary
    /// lifetime), self-deadlocking the runtime — postgres hits this
    /// constantly (every backend OPENs pg_internal.init). 512 seeds of
    /// one shared path make a same-shard collision near-certain; run
    /// under a watchdog so the failure mode is a panic, not a hang.
    #[test]
    fn seed_open_fd_shared_path_does_not_deadlock() {
        let (handler, _fh_mgr, temp) = create_test_handler();
        let target = temp.path().join("shared.dat");
        std::fs::write(&target, b"seed me").unwrap();

        let file = Arc::new(std::fs::File::open(&target).unwrap());
        handler.test_seed_fd([1u8; 12], file, target.clone(), false);

        let handler = std::sync::Arc::new(handler);
        let h = std::sync::Arc::clone(&handler);
        let path = target.clone();
        let worker = std::thread::spawn(move || {
            for i in 0u16..512 {
                let mut other = [0u8; 12];
                other[..2].copy_from_slice(&i.to_le_bytes());
                other[2] = 0x5e;
                let sid = StateId { seqid: 1, other };
                h.seed_open_fd(&sid, &path, false);
            }
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while !worker.is_finished() {
            assert!(
                std::time::Instant::now() < deadline,
                "seed_open_fd deadlocked on a same-shard iter+insert"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        worker.join().unwrap();
        // Every distinct stateid now maps to the shared fd.
        assert_eq!(handler.fd_cache.len(), 513);
    }

    /// A10: with the space model exhausted (reserve > any disk),
    /// WRITE and OPEN-create under its root answer NOSPC — while
    /// opening an EXISTING file still serves (reads must flow at any
    /// fullness). Scoped by root, so no other test's I/O is touched;
    /// the retry loop absorbs a concurrent test swapping the global
    /// space install.
    #[tokio::test]
    async fn write_and_create_refused_nospc_when_headroom_exhausted() {
        let (handler, fh_mgr, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);
        let export_fh = fh_mgr.path_to_filehandle(fh_mgr.get_export_path()).unwrap();
        ctx.current_fh = Some(export_fh.clone());
        let open = |name: &str| OpenOp {
            seqid: 0,
            share_access: OPEN4_SHARE_ACCESS_BOTH,
            share_deny: OPEN4_SHARE_DENY_NONE,
            owner: b"nospc-owner".to_vec(),
            openhow: OpenHow::Create(Fattr4 { attrmask: vec![], attr_vals: vec![] }),
            claim: OpenClaim::Null(name.to_string()),
        };

        // Created BEFORE the exhausted model installs. OPEN moves
        // ctx.current_fh to the file — keep both handles and re-set
        // per op (OPEN resolves names against the DIRECTORY fh).
        let pre = handler.handle_open(open("nospc-pre.bin"), &mut ctx).await;
        assert_eq!(pre.status, Nfs4Status::Ok);
        let stateid = pre.stateid.unwrap();
        let file_fh = ctx.current_fh.clone();

        let scfg = crate::tier::space::SpaceConfig {
            root: fh_mgr.get_export_path().to_path_buf(),
            reserve_bytes: u64::MAX, // headroom 0 on any real disk
            watermark_pct: 85,
            ballast_path: None,
            ballast_bytes: 0,
        };
        let mut ok = false;
        for _ in 0..50 {
            crate::tier::space::configure(scfg.clone()).unwrap();
            ctx.current_fh = file_fh.clone();
            let w = handler
                .handle_write(
                    WriteOp {
                        stateid: stateid.clone(),
                        offset: 0,
                        stable: FILE_SYNC4,
                        data: Bytes::from_static(b"refused"),
                    },
                    &ctx,
                )
                .await;
            ctx.current_fh = Some(export_fh.clone());
            let c = handler.handle_open(open("nospc-new.bin"), &mut ctx).await;
            ctx.current_fh = Some(export_fh.clone());
            let e = handler.handle_open(open("nospc-pre.bin"), &mut ctx).await;
            if w.status == Nfs4Status::NoSpc
                && c.status == Nfs4Status::NoSpc
                && e.status == Nfs4Status::Ok
            {
                assert_eq!(w.count, 0);
                ok = true;
                break;
            }
        }
        assert!(
            ok,
            "exhausted headroom must refuse WRITE + OPEN-create with NOSPC \
             and still open existing files"
        );
        assert!(
            !fh_mgr.get_export_path().join("nospc-new.bin").exists(),
            "a refused create must leave nothing behind"
        );
    }

    /// Step 10 (C2/C6): READ and WRITE of an EVICTED file answer
    /// NFS4ERR_DELAY — never the stub's size-based EOF, never a byte
    /// landed in the stub, never a capture mark.
    #[tokio::test]
    async fn read_and_write_answer_delay_on_an_evicted_file() {
        use std::os::unix::fs::MetadataExt;
        crate::tier::capture::force_enable();
        let (handler, fh_mgr, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);
        let export_fh = fh_mgr.path_to_filehandle(fh_mgr.get_export_path()).unwrap();
        ctx.current_fh = Some(export_fh);
        let open_res = handler
            .handle_open(
                OpenOp {
                    seqid: 0,
                    share_access: OPEN4_SHARE_ACCESS_BOTH,
                    share_deny: OPEN4_SHARE_DENY_NONE,
                    owner: b"evicted-owner".to_vec(),
                    openhow: OpenHow::Create(Fattr4 { attrmask: vec![], attr_vals: vec![] }),
                    claim: OpenClaim::Null("evicted-lane.bin".to_string()),
                },
                &mut ctx,
            )
            .await;
        assert_eq!(open_res.status, Nfs4Status::Ok);
        let stateid = open_res.stateid.unwrap();
        let path = fh_mgr.get_export_path().join("evicted-lane.bin");
        std::fs::write(&path, b"real data").unwrap();
        let md = std::fs::metadata(&path).unwrap();
        let (dev, ino) = (md.dev(), md.ino());
        crate::tier::capture::forget(dev, ino);

        // Evict it (marker only — the truncated-stub shape).
        std::fs::OpenOptions::new().write(true).open(&path).unwrap().set_len(0).unwrap();
        crate::tier::evict::install_marker_for_tests(dev, ino, 9);

        let r = handler
            .handle_read(ReadOp { stateid: stateid.clone(), offset: 0, count: 9 }, &ctx)
            .await;
        assert_eq!(r.status, Nfs4Status::Delay, "READ of evicted must DELAY, not serve EOF");
        assert!(r.data.is_empty());

        let w = handler
            .handle_write(
                WriteOp {
                    stateid: stateid.clone(),
                    offset: 0,
                    stable: FILE_SYNC4,
                    data: Bytes::from_static(b"zzz"),
                },
                &ctx,
            )
            .await;
        assert_eq!(w.status, Nfs4Status::Delay, "WRITE to evicted must DELAY (C6)");
        assert_eq!(w.count, 0);
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            0,
            "no byte may land in the stub"
        );
        assert!(
            crate::tier::capture::snapshot(dev, ino).is_none_or(|c| !c.is_dirty()),
            "a refused WRITE must note nothing"
        );
        crate::tier::evict::forget(dev, ino);
    }

    fn create_test_handler() -> (IoOperationHandler, Arc<FileHandleManager>, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let export_path = temp_dir.path().to_path_buf();
        
        // Create a test file for I/O tests
        std::fs::write(export_path.join("testfile.txt"), b"test data for reading").unwrap();
        
        let fh_mgr = Arc::new(FileHandleManager::new(export_path));
        let state_mgr = Arc::new(StateManager::new_in_memory(""));
        let handler = IoOperationHandler::new(state_mgr, fh_mgr.clone());
        (handler, fh_mgr, temp_dir)
    }

    #[tokio::test]
    async fn test_open() {
        let (handler, fh_mgr, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);

        // Set current filehandle
        ctx.current_fh = Some(fh_mgr.get_root_fh().unwrap());

        let op = OpenOp {
            seqid: 0,
            share_access: OPEN4_SHARE_ACCESS_READ,
            share_deny: OPEN4_SHARE_DENY_NONE,
            owner: b"test-owner".to_vec(),
            openhow: OpenHow::NoCreate,
            claim: OpenClaim::Fh,
        };

        let res = handler.handle_open(op, &mut ctx).await;
        assert_eq!(res.status, Nfs4Status::Ok);
        assert!(res.stateid.is_some());
        assert_eq!(res.delegation, OpenDelegationType::None);
    }

    #[tokio::test]
    async fn test_open_close() {
        let (handler, fh_mgr, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);

        // Set current filehandle
        ctx.current_fh = Some(fh_mgr.get_root_fh().unwrap());

        // OPEN
        let open_op = OpenOp {
            seqid: 0,
            share_access: OPEN4_SHARE_ACCESS_READ,
            share_deny: OPEN4_SHARE_DENY_NONE,
            owner: b"test-owner".to_vec(),
            openhow: OpenHow::NoCreate,
            claim: OpenClaim::Fh,
        };

        let open_res = handler.handle_open(open_op, &mut ctx).await;
        assert_eq!(open_res.status, Nfs4Status::Ok);
        let stateid = open_res.stateid.unwrap();

        // CLOSE
        let close_op = CloseOp {
            seqid: 0,
            stateid,
        };

        let close_res = handler.handle_close(close_op, &ctx);
        assert_eq!(close_res.status, Nfs4Status::Ok);
        assert!(close_res.stateid.is_some());
    }

    #[tokio::test]
    async fn test_read() {
        let (handler, fh_mgr, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);

        // Get filehandle for the test file we created
        let test_file_path = fh_mgr.get_export_path().join("testfile.txt");
        let test_fh = fh_mgr.path_to_filehandle(&test_file_path).unwrap();
        ctx.current_fh = Some(test_fh);

        // Open first
        let open_op = OpenOp {
            seqid: 0,
            share_access: OPEN4_SHARE_ACCESS_READ,
            share_deny: OPEN4_SHARE_DENY_NONE,
            owner: b"test-owner".to_vec(),
            openhow: OpenHow::NoCreate,
            claim: OpenClaim::Fh,
        };

        let open_res = handler.handle_open(open_op, &mut ctx).await;
        let stateid = open_res.stateid.unwrap();

        // READ
        let read_op = ReadOp {
            stateid,
            offset: 0,
            count: 1024,
        };

        let read_res = handler.handle_read(read_op, &ctx).await;
        assert_eq!(read_res.status, Nfs4Status::Ok);
    }

    #[tokio::test]
    async fn test_write() {
        let (handler, fh_mgr, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);

        // Get filehandle for the test file
        let test_file_path = fh_mgr.get_export_path().join("testfile.txt");
        let test_fh = fh_mgr.path_to_filehandle(&test_file_path).unwrap();
        ctx.current_fh = Some(test_fh);

        // Open first
        let open_op = OpenOp {
            seqid: 0,
            share_access: OPEN4_SHARE_ACCESS_WRITE,
            share_deny: OPEN4_SHARE_DENY_NONE,
            owner: b"test-owner".to_vec(),
            openhow: OpenHow::NoCreate,
            claim: OpenClaim::Fh,
        };

        let open_res = handler.handle_open(open_op, &mut ctx).await;
        let stateid = open_res.stateid.unwrap();

        // WRITE
        let write_op = WriteOp {
            stateid,
            offset: 0,
            stable: UNSTABLE4,
            data: Bytes::from("hello world"),
        };

        let write_res = handler.handle_write(write_op, &ctx).await;
        assert_eq!(write_res.status, Nfs4Status::Ok);
        assert_eq!(write_res.count, 11);
    }

    /// POSIX checks permission at OPEN, not per write: git fchmods
    /// every loose object 0444 BEFORE close, and a small file's dirty
    /// pages flush LAST — so the first WRITE RPC can arrive after the
    /// chmod. If the stateid-keyed fd lookup misses (path-form
    /// mismatch, eviction), the lazy re-open gets EACCES; the write
    /// must then be served from any open-time fd of the same inode,
    /// never surfaced as EIO to a legally-open writer.
    #[tokio::test]
    async fn write_after_chmod_readonly_served_from_open_time_fd() {
        let (handler, fh_mgr, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);

        let test_file_path = fh_mgr.get_export_path().join("testfile.txt");
        let test_fh = fh_mgr.path_to_filehandle(&test_file_path).unwrap();
        ctx.current_fh = Some(test_fh);

        let open_res = handler.handle_open(OpenOp {
            seqid: 0,
            share_access: OPEN4_SHARE_ACCESS_WRITE,
            share_deny: OPEN4_SHARE_DENY_NONE,
            owner: b"chmod-owner".to_vec(),
            openhow: OpenHow::NoCreate,
            claim: OpenClaim::Fh,
        }, &mut ctx).await;
        let stateid = open_res.stateid.unwrap();

        // Manufacture the miss the wild produces via path-form mismatch
        // or eviction: the stateid's own entry is gone, but a writable
        // open-time fd for the inode survives under another key.
        let seeded = handler.fd_cache.remove(&stateid.other)
            .expect("OPEN should have seeded an fd (F17c)");
        handler.fd_cache.insert([0xAB; 12], CachedFile {
            file: Arc::clone(&seeded.file),
            path: seeded.path.clone(),
            writable: seeded.writable,
            ino: seeded.ino,
        });
        assert!(seeded.writable, "seed must be a write-open fd");

        // The chmod lands before the flush, as git does it.
        let mut perms = std::fs::metadata(&test_file_path).unwrap().permissions();
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o444);
        std::fs::set_permissions(&test_file_path, perms).unwrap();

        let write_res = handler.handle_write(WriteOp {
            stateid,
            offset: 0,
            stable: UNSTABLE4,
            data: Bytes::from("flush after chmod"),
        }, &ctx).await;
        assert_eq!(write_res.status, Nfs4Status::Ok,
            "flush of a valid write-open must survive a prior chmod 0444");
        assert_eq!(write_res.count, 17);

        // Restore mode so TempDir cleanup can delete the file.
        let mut perms = std::fs::metadata(&test_file_path).unwrap().permissions();
        perms.set_mode(0o644);
        std::fs::set_permissions(&test_file_path, perms).unwrap();
    }

    #[tokio::test]
    async fn test_commit() {
        let (handler, fh_mgr, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);

        // Get filehandle for the test file
        let test_file_path = fh_mgr.get_export_path().join("testfile.txt");
        let test_fh = fh_mgr.path_to_filehandle(&test_file_path).unwrap();
        ctx.current_fh = Some(test_fh);

        // COMMIT
        let commit_op = CommitOp {
            offset: 0,
            count: 0, // 0 means commit entire file
        };

        let commit_res = handler.handle_commit(commit_op, &ctx).await;
        assert_eq!(commit_res.status, Nfs4Status::Ok);
    }

    #[tokio::test]
    async fn test_open_with_file_creation() {
        let (handler, fh_mgr, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);

        // Set current filehandle to export root (parent directory for creation)
        let export_fh = fh_mgr.path_to_filehandle(fh_mgr.get_export_path()).unwrap();
        ctx.current_fh = Some(export_fh);

        let op = OpenOp {
            seqid: 0,
            share_access: OPEN4_SHARE_ACCESS_WRITE,
            share_deny: OPEN4_SHARE_DENY_NONE,
            owner: b"test-owner".to_vec(),
            openhow: OpenHow::Create(Fattr4 { attrmask: vec![], attr_vals: vec![] }),
            claim: OpenClaim::Null("new-file.txt".to_string()),
        };

        let res = handler.handle_open(op, &mut ctx).await;
        
        // Should succeed and create the file
        assert_eq!(res.status, Nfs4Status::Ok);
        assert!(res.stateid.is_some());
        
        // Verify current filehandle was updated to the new file
        assert!(ctx.current_fh.is_some());
        
        // Verify file exists on disk
        let file_path = fh_mgr.resolve_handle(ctx.current_fh.as_ref().unwrap()).unwrap();
        assert!(file_path.exists());
        assert_eq!(file_path.file_name().unwrap().to_str().unwrap(), "new-file.txt");
    }

    #[tokio::test]
    async fn test_write_rejects_unknown_stateid() {
        // RFC 8881 §16.2.3.1 / §8.2.2: WRITE accepts the special bypass
        // stateids, the "current stateid" form (seqid=0 with a known
        // `other`), or an exact seqid match. A stateid with an `other`
        // that the server has never seen MUST be rejected as
        // NFS4ERR_BAD_STATEID — accepting it (as the previous "relaxed"
        // implementation did) was a write-share-deny bypass.
        let (handler, fh_mgr, temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);

        let test_file = temp.path().join("test-write.txt");
        std::fs::File::create(&test_file).unwrap();
        ctx.current_fh = Some(fh_mgr.path_to_filehandle(&test_file).unwrap());

        // Unknown `other` — never allocated.
        let bogus = StateId { seqid: 0, other: [0xAB; 12] };
        let write_op = WriteOp {
            stateid: bogus,
            offset: 0,
            stable: UNSTABLE4,
            data: Bytes::from("test data"),
        };

        let write_res = handler.handle_write(write_op, &ctx).await;
        assert_eq!(write_res.status, Nfs4Status::BadStateId);
        assert_eq!(write_res.count, 0);
    }

    #[tokio::test]
    async fn test_read_with_relaxed_stateid_validation() {
        let (handler, fh_mgr, temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);

        // Create a test file with content
        let test_file = temp.path().join("test-read.txt");
        std::fs::write(&test_file, b"test content").unwrap();
        
        // Set current filehandle to the test file
        ctx.current_fh = Some(fh_mgr.path_to_filehandle(&test_file).unwrap());

        // Allocate a stateid
        let stateid = handler.state_mgr.stateids.allocate(
            StateType::Open,
            1,
            Some(ctx.current_fh.as_ref().unwrap().data.clone()),
        );

        // Test READ with seqid=0
        let read_op = ReadOp {
            stateid: StateId {
                seqid: 0,  // Relaxed validation should accept this
                other: stateid.other,
            },
            offset: 0,
            count: 100,
        };

        let read_res = handler.handle_read(read_op, &ctx).await;
        
        // Should succeed
        assert_eq!(read_res.status, Nfs4Status::Ok);
        assert_eq!(read_res.data.as_ref(), b"test content");
    }

    #[tokio::test]
    async fn test_open_without_create() {
        let (handler, fh_mgr, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);

        // Set current filehandle
        ctx.current_fh = Some(fh_mgr.root_filehandle().unwrap());

        let op = OpenOp {
            seqid: 0,
            share_access: OPEN4_SHARE_ACCESS_READ,
            share_deny: OPEN4_SHARE_DENY_NONE,
            owner: b"test-owner".to_vec(),
            openhow: OpenHow::NoCreate,
            claim: OpenClaim::Null("nonexistent.txt".to_string()),
        };

        let res = handler.handle_open(op, &mut ctx).await;
        
        // Should succeed (we don't validate file existence for NoCreate)
        assert_eq!(res.status, Nfs4Status::Ok);
        assert!(res.stateid.is_some());
    }

    #[tokio::test]
    async fn test_full_write_workflow() {
        let (handler, fh_mgr, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);

        // Set current filehandle to export root (parent directory for file creation)
        let export_fh = fh_mgr.path_to_filehandle(fh_mgr.get_export_path()).unwrap();
        ctx.current_fh = Some(export_fh);

        // 1. OPEN with create (will create a NEW file)
        let open_op = OpenOp {
            seqid: 0,
            share_access: OPEN4_SHARE_ACCESS_BOTH,
            share_deny: OPEN4_SHARE_DENY_NONE,
            owner: b"test-owner".to_vec(),
            openhow: OpenHow::Create(Fattr4 { attrmask: vec![], attr_vals: vec![] }),
            claim: OpenClaim::Null("workflow-test.txt".to_string()),
        };

        let open_res = handler.handle_open(open_op, &mut ctx).await;
        assert_eq!(open_res.status, Nfs4Status::Ok);
        let stateid = open_res.stateid.unwrap();

        // 2. WRITE data — use the open stateid as-is. WRITE now requires an
        // exact seqid (RFC 5661 §18.32); the previous test mutated seqid to
        // 0 to exercise the now-removed relaxed path.
        let write_op = WriteOp {
            stateid,
            offset: 0,
            stable: FILE_SYNC4,
            data: Bytes::from("Hello, NFS!"),
        };

        let write_res = handler.handle_write(write_op, &ctx).await;
        assert_eq!(write_res.status, Nfs4Status::Ok);
        assert_eq!(write_res.count, 11);

        // 3. READ data back. READ still allows ANONYMOUS_STATEID and the
        // current/previous seqid, so we can re-use the open stateid.
        let read_op = ReadOp {
            stateid,
            offset: 0,
            count: 100,
        };

        let read_res = handler.handle_read(read_op, &ctx).await;
        assert_eq!(read_res.status, Nfs4Status::Ok);
        assert_eq!(read_res.data.as_ref(), b"Hello, NFS!");

        // 4. CLOSE
        let close_op = CloseOp {
            seqid: 0,
            stateid,
        };

        let close_res = handler.handle_close(close_op, &ctx);
        assert_eq!(close_res.status, Nfs4Status::Ok);
    }

    #[test]
    fn read_only_open_grants_no_delegation_by_default() {
        // Delegations are gated off (FLINT_NFS_DELEGATIONS unset): a
        // conflict-free read-only open of an existing file — the case the
        // grant path fires on — must yield OPEN_DELEGATE_NONE and mint no
        // server-side delegation record. Granting without a working
        // CB_RECALL path would let a client cache stale data forever.
        let (handler, fh_mgr, _temp) = create_test_handler();

        assert!(!delegations_enabled());
        let delegation = handler.try_grant_read_delegation(
            1,
            &fh_mgr.get_root_fh().unwrap(),
            OPEN4_SHARE_ACCESS_READ,
        );
        assert_eq!(delegation, OpenDelegationType::None);
    }
}
