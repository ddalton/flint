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
use crate::nfs::v4::compound::{CompoundContext, WhyNoDelegation};
use crate::nfs::v4::state::StateManager;
use crate::nfs::v4::operations::fileops::Fattr4;
use crate::nfs::v4::filehandle::FileHandleManager;
use bytes::Bytes;
use super::fd_cache::{CachedFile, FdCache};
use std::fs::File;
use std::path::PathBuf;
use std::sync::Arc;
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

    /// CLAIM_PREVIOUS (1): reclaim during grace. The CFH is the file
    /// (same resolution as `Fh`); carries the open_delegation_type4 the
    /// client says it held before the restart, so the delegation half
    /// of the reply is answered DELIBERATELY — always NONE today:
    /// delegations are never retained across a restart, and declining a
    /// READ-delegation reclaim is legal and loses nothing (RFC 8881
    /// §10.4). Before this variant the value was decoded and discarded.
    Previous { delegate_type: u32 },

    /// CLAIM_DELEGATE_CUR (2) / CLAIM_DELEG_CUR_FH (5): a conversion
    /// open — the client turning a locally-cached open under a
    /// delegation into a real open stateid (what Linux does on recall,
    /// before DELEGRETURN). `file` is Some for the 4.0-style claim 2
    /// (name relative to the CFH directory), None for claim 5 (the CFH
    /// is the file). The presented delegation stateid MUST validate:
    /// these claims used to collapse to `Fh` with the stateid dropped,
    /// which for claim 2 executed the open against the PARENT DIRECTORY.
    DelegCur { stateid: StateId, file: Option<String> },
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
    /// The OPEN reply's delegation arm, ready to encode:
    /// `Read` for a granted READ delegation (the only kind flint
    /// grants), `NoneExt` for "none, and here is why" when the client
    /// set a WANT bit, and `None` for a plain OPEN_DELEGATE_NONE.
    pub delegation: Option<crate::nfs::v4::compound::Delegation>,
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
    /// A `Segment`, not `Bytes`: the payload may be staged in a pipe and
    /// never enter userspace at all. See `crate::nfs::segment`.
    pub data: crate::nfs::segment::Segment,
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

    /// Step 10/11's writable-open probe: does any cached fd hold this
    /// inode writable? (A4: open-hot files are non-evictable. The
    /// cache reflects live opens; the gate + marker consults are the
    /// correctness fences — this is eviction POLICY, keeping hot files
    /// from thrashing evict/hydrate.)
    pub fn has_writable_ino(&self, ino: u64) -> bool {
        self.fd_cache.find_by_ino(ino, true).is_some()
    }
}

/// Whether an fd may be cached under this stateid. Special stateids
/// (`other` all-zeros / all-ones, RFC 8881 §8.2.3) are not unique to
/// one open — caching under them would alias different files to the
/// same key and serve one file's fd for another's I/O.
fn cacheable_stateid(other: &[u8; 12]) -> bool {
    *other != [0u8; 12] && *other != [0xffu8; 12]
}

// The FLINT_NFS_DELEGATIONS gate lives with the delegation state core.
use crate::nfs::v4::state::delegations_enabled;

/// (dev,ino) of an opened file, for the open-identity index the
/// delegation grant predicate reads (`file_has_write_open` — design §4
/// rule 5). Best-effort: a failed stat skips the indexing, and the
/// only consequence is that no delegation is granted on the file this
/// cycle — refusal is free by design.
fn open_ident_of(md: std::io::Result<std::fs::Metadata>) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    md.ok().map(|m| (m.dev(), m.ino()))
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

/// Run the READ body inline on the async worker instead of handing it to
/// the blocking pool. **EXPERIMENT, default off.**
///
/// Why it exists: post-`session_limits`, flint uses only 1.14x knfsd's CPU
/// but runs at 0.819 its throughput, and idles MORE than knfsd while
/// being slower (76% vs 80% busy). It is latency-bound, not CPU-bound,
/// and it does 17.0 context switches per READ against knfsd's 7.1. The
/// `spawn_blocking` round trip -- enqueue, wake a pool thread, run, wake
/// the worker back -- is the prime suspect for that gap, and a CPU
/// profile CANNOT see it (it cost 3.5% of CPU, which is why measuring
/// the wrong axis made me dismiss it once already).
///
/// NOT SAFE AS A DEFAULT: a cold read here blocks a runtime worker. This
/// exists to size the prize on a page-cache-warm workload. If it is
/// worth having, the shipping form is io_uring or `block_in_place`, not
/// this.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ReadMode {
    /// Hand the read to the blocking pool. Safe everywhere; costs a task
    /// enqueue and two wakeups per READ.
    SpawnBlocking,
    /// Run it on the async worker. Fastest, and NOT SAFE as a default --
    /// a cold read stalls a runtime worker.
    Inline,
    /// Run it on the async worker but tell the runtime first, so siblings
    /// migrate to another thread. No task handoff, no stalled worker.
    /// Requires the multi-thread runtime -- every NFS server binary
    /// (`nfs_mds_main`, `nfs_main`, `nfs_ds_main`) builds one, but
    /// `#[tokio::test]` does NOT, which is why this is never the default
    /// under test.
    BlockInPlace,
}

/// `BlockInPlace` by default on a multi-thread runtime, which every NFS
/// server binary builds.
///
/// Measured, 4 readers, O_DIRECT, warm, splice on, three arms
/// interleaved in one session with a page-cache guard:
///
/// | arm | cpu-ms/GiB | MiB/s | vs knfsd |
/// |---|---|---|---|
/// | spawn_blocking | 355 | 4240 | 0.712 |
/// | **block_in_place** | **310** | **5292** | **0.861** |
/// | knfsd | 280 | 6006 | 1.000 |
///
/// **+24.2% throughput.** The win is latency, not CPU: `spawn_blocking`
/// costs a task enqueue and two wakeups per READ, and with N synchronous
/// readers throughput is N x rsize / per-request latency. A CPU profile
/// cannot see this — the scheduler cost was 3.5% of CPU, which is why
/// measuring the wrong axis dismissed it once.
///
/// **A pynfs COUR6 SCARE, and what it actually was.**
/// `st_courtesy.testShareReservationDB03` failed 3 of 8 full-suite runs
/// under this mode and 0 of 4 under the pool, which looked like a
/// mode-dependent regression and got the default reverted once. It was
/// the RIG. The 2 GB test VM had accumulated **seven orphaned `tcpdump`
/// processes holding 1.37 GB** — nfstest capture buffers from earlier
/// runs — leaving ~33 MB free. Re-run on the cleaned VM, alternating the
/// two modes over six full suites: **block_in_place 3/3 and pool 3/3,
/// both 170/1.**
///
/// Worth keeping because the false signal was so convincing: a
/// same-binary A/B minutes apart still showed 169/2 vs 170/1, and I
/// treated that as decisive. Under memory starvation a 10s margin on a
/// 90s lease is not a margin. **Check `ps -eo rss,comm --sort=-rss` on
/// that VM before trusting any timing-sensitive result**; `tpacket_rcv`
/// appearing in a perf profile is the tell.
///
/// `block_in_place` PANICS on a current-thread runtime, which is exactly
/// what `#[tokio::test]` builds, so the flavor is checked rather than
/// assumed and tests keep the pool path.
/// `FLINT_NFS_INLINE_READ` forces: 0 pool, 1 inline, 2 block-in-place.
/// Inline measured +8.5% and is NOT safe to default either — it stalls a
/// worker on a cold read with no migration.
fn read_mode() -> ReadMode {
    static M: std::sync::OnceLock<ReadMode> = std::sync::OnceLock::new();
    *M.get_or_init(|| match std::env::var("FLINT_NFS_INLINE_READ").as_deref() {
        Ok("0") | Ok("false") | Ok("no") => ReadMode::SpawnBlocking,
        Ok("1") | Ok("true") | Ok("yes") => ReadMode::Inline,
        Ok("2") | Ok("block_in_place") => ReadMode::BlockInPlace,
        _ => match tokio::runtime::Handle::try_current().map(|h| h.runtime_flavor()) {
            Ok(tokio::runtime::RuntimeFlavor::MultiThread) => ReadMode::BlockInPlace,
            _ => ReadMode::SpawnBlocking,
        },
    })
}

#[derive(Clone, Copy)]
enum WriteMode {
    SpawnBlocking,
    BlockInPlace,
}

/// How the UNSTABLE write body runs. An UNSTABLE write is a page-cache
/// pwrite (µs-scale); the `spawn_blocking` round trip costs more than
/// the work it dispatches — 6.5 futex calls per WRITE measured under a
/// 4-writer O_DIRECT load, the same enqueue/wake/complete shape the READ
/// path shed in `f2a950e9` (+24%) and the DS lane never had
/// (`ds/io.rs::fast_blocking`). DATA_SYNC/FILE_SYNC writes pay an fsync
/// (ms-scale) and never come through here — they stay on the blocking
/// pool so a migrated worker is not monopolised under pipelined dispatch.
///
/// `block_in_place` PANICS on a current-thread runtime (what
/// `#[tokio::test]` builds), so the flavor is checked rather than
/// assumed. `FLINT_NFS_INLINE_WRITE` forces: 0 pool, 2 block-in-place.
fn write_mode() -> WriteMode {
    static M: std::sync::OnceLock<WriteMode> = std::sync::OnceLock::new();
    *M.get_or_init(|| match std::env::var("FLINT_NFS_INLINE_WRITE").as_deref() {
        Ok("0") | Ok("false") | Ok("no") => WriteMode::SpawnBlocking,
        Ok("2") | Ok("block_in_place") => WriteMode::BlockInPlace,
        _ => match tokio::runtime::Handle::try_current().map(|h| h.runtime_flavor()) {
            Ok(tokio::runtime::RuntimeFlavor::MultiThread) => WriteMode::BlockInPlace,
            _ => WriteMode::SpawnBlocking,
        },
    })
}

impl IoOperationHandler {
    /// Create a new I/O operation handler
    pub fn new(state_mgr: Arc<StateManager>, fh_mgr: Arc<FileHandleManager>) -> Self {
        // The write verifier exists for exactly one purpose: telling a
        // client its UNSTABLE writes did not survive a server restart
        // (RFC 8881 §18.32.3) so it re-sends them. That gives it two
        // obligations pulling opposite ways: CONSTANT within one
        // process lifetime (see write_verifier() — a per-call value is
        // the Linux 6.8 COPY infinite loop), and DISTINCT across any
        // two lifetimes. Wall-clock SECONDS delivered neither robustly:
        // a supervised restart completing within the same second
        // re-minted an IDENTICAL verifier, the client matched COMMIT
        // against it, dropped its dirty pages, and the never-resent
        // data was gone — silently, with no wire anomaly to observe
        // (kubelet's first restart has no backoff, so sub-second is the
        // COMMON case, not the corner). Nanoseconds mixed with fresh
        // per-process entropy make a repeat across incarnations
        // impossible in practice, clock steps included.
        let write_verifier = {
            use std::time::{SystemTime, UNIX_EPOCH};
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos() as u64;
            nanos ^ rand::random::<u64>()
        };

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
        use crate::nfs::v4::open_beneath;
        let opened = open_beneath::open(
            std::fs::OpenOptions::new().read(true).write(true),
            path,
        )
        .map(|f| (f, true))
        .or_else(|_| open_beneath::open_read(path).map(|f| (f, false)));
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
    /// The path this handler WOULD serve bytes from when
    /// `resolve_handle` fails — i.e. the second and last of its two
    /// byte-producing routes (the first being a resolvable path).
    ///
    /// The pNFS fallback gate consults this so its striped-file check
    /// covers every route that can produce bytes. Without it the gate
    /// asked only about the resolvable path, and an unresolvable
    /// handle skipped the check entirely while READ still served the
    /// cached fd — which on an MDS is the file's SPARSE STUB, i.e.
    /// zeros with NFS4_OK. (dispatcher.rs `stub_io_disposition`.)
    pub fn fallback_serve_path(
        &self,
        fh: &crate::nfs::v4::protocol::Nfs4FileHandle,
        stateid_other: &[u8; 12],
        want_writable: bool,
    ) -> Option<PathBuf> {
        self.stale_open_fallback(fh, stateid_other, want_writable)
            .map(|(p, _)| p)
    }

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
    /// Per-client stateid quota (B6). Checked at OPEN's two mint arms —
    /// stateids are minted by unauthenticated wire ops and each costs
    /// memory plus a persisted state.db row. DELAY: CLOSE and lease
    /// expiry free capacity, so a legitimate client retries through.
    fn stateid_quota_exhausted(&self, client_id: u64) -> bool {
        let held = self.state_mgr.stateids.count_for_client(client_id);
        let max = self.state_mgr.quotas.max_stateids_per_client;
        if held >= max {
            warn!(
                "OPEN: client {client_id} at stateid quota ({held}/{max}) — DELAY \
                 (FLINT_NFS_MAX_STATEIDS_PER_CLIENT)"
            );
            return true;
        }
        false
    }

    /// The mutator identity a fence consult reports (design §5.2 site
    /// 10): the session's client, or None for sessionless lanes (the
    /// in-process file API, v4.0 paths) — where every delegation
    /// holder is "another client" and must be recalled with DELAY.
    /// Distinct from `get_client_id_from_context`, whose fallback of 1
    /// could collide with a real client id and hand that client a
    /// self-conflict carve-out it has no right to.
    fn fence_mutator(&self, ctx: &CompoundContext) -> Option<u64> {
        ctx.session_id
            .as_ref()
            .and_then(|sid| self.state_mgr.sessions.get_session(sid))
            .map(|s| s.client_id)
    }

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
        let mut op = op;
        debug!("OPEN: share_access=0x{:08x}, share_deny=0x{:08x}",
               op.share_access, op.share_deny);
        debug!("OPEN: openhow={:?}, claim={:?}", op.openhow, op.claim);

        // Grant rule 3 (design §4) wants the claim AS SENT: only
        // CLAIM_NULL / CLAIM_FH opens are grantable, and the
        // normalization below rewrites reclaim/conversion claims into
        // those very shapes.
        let claim_grantable = matches!(&op.claim, OpenClaim::Null(_) | OpenClaim::Fh);

        // Normalize the delegation-flavoured claims up front so the rest
        // of the handler keeps its two resolution shapes (by-name /
        // by-CFH). Order matters: a conversion open validates its
        // delegation stateid BEFORE any path work.
        match &op.claim {
            OpenClaim::Previous { delegate_type } => {
                // Reclaim: the dispatcher's grace gate already admitted
                // it; the CFH is the file, so resolution is the Fh shape.
                // The delegation half of the reclaim stays NONE (we never
                // retain delegations across restart — declining a READ
                // reclaim is legal and the client just revalidates).
                debug!("OPEN: CLAIM_PREVIOUS reclaim, claimed delegate_type={}", delegate_type);
                op.claim = OpenClaim::Fh;
            }
            OpenClaim::DelegCur { stateid, file } => {
                match self.state_mgr.delegations.lookup(stateid) {
                    None => {
                        // No live delegation by that stateid — the only
                        // possible answer while the server never grants,
                        // and the correct one for a bogus/foreign stateid
                        // once it does.
                        warn!("OPEN: conversion claim with unknown delegation stateid {:?}", stateid);
                        return OpenRes {
                            status: Nfs4Status::BadStateId,
                            stateid: None,
                            change_info: None,
                            result_flags: 0,
                            delegation: None,
                            attrset: vec![],
                        };
                    }
                    Some((_client, _path)) => {
                        // A live delegation exists (grants are not
                        // implemented yet, so this arm is unreachable
                        // today; the full validation — owner match, file
                        // match, fence exemption during recall — lands
                        // with the grant/recall machine).
                        op.claim = match file {
                            Some(name) => OpenClaim::Null(name.clone()),
                            None => OpenClaim::Fh,
                        };
                    }
                }
            }
            OpenClaim::Null(_) | OpenClaim::Fh => {}
        }

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
                    delegation: None,
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
                        delegation: None,
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
            // Normalized away at the top of the handler.
            OpenClaim::Previous { .. } | OpenClaim::DelegCur { .. } => String::new(),
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
                        delegation: None,
                        attrset: vec![],
                    };
                }
            };

            // Build full file path
            let file_path = parent_path.join(&filename);
            debug!("OPEN: Creating file at {:?}", file_path);

            // RFC 8881 §18.16.3: OPEN of a symbolic link is
            // NFS4ERR_SYMLINK — the client READLINKs and re-resolves in
            // its own namespace. Answering early matters here because
            // the create path admits against the space reserve and
            // stamps ownership on the way to its open; none of that
            // should happen for a request that cannot succeed. The
            // BINDING guarantee is `open_beneath`'s O_NOFOLLOW below,
            // which cannot be raced — this check only buys the right
            // error and an untouched filesystem.
            if crate::nfs::v4::open_beneath::leaf_is_symlink(&file_path) {
                warn!("OPEN(create): {:?} is a symlink → SYMLINK", file_path);
                return OpenRes {
                    status: Nfs4Status::SymLink,
                    stateid: None,
                    change_info: None,
                    result_flags: 0,
                    delegation: None,
                    attrset: vec![],
                };
            }

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
                                    delegation: None,
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
                                delegation: None,
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
                    delegation: None,
                    attrset: vec![],
                };
            }
            // ── §2A leg A3: OPEN authorization ──────────────────────
            //
            // 82 of the 645 pjdfstest assertions flint fails and knfsd
            // passes are here — `open FILE O_RDONLY, expected EACCES,
            // got 0`. Any client could open any file regardless of its
            // mode, because the only check was the kernel's, performed
            // as the SERVER's identity.
            //
            // An OPEN that CREATES is permission on the parent directory
            // (write+execute); an OPEN of an existing file is permission
            // on the file, in the directions the share reservation asks
            // for. share_access: 1 = READ, 2 = WRITE, 3 = BOTH.
            {
                use crate::nfs::v4::authz;
                let cred = ctx.cred();
                if existed {
                    if let Ok(md) = crate::nfs::v4::stat_cache::stat(&file_path) {
                        let mut want = 0u32;
                        if op.share_access & 1 != 0 { want |= authz::R; }
                        if op.share_access & 2 != 0 { want |= authz::W; }
                        if want != 0 {
                            if let Err(st) =
                                authz::check(cred.as_ref(), &md, want, "OPEN", &file_path)
                            {
                                return OpenRes {
                                    status: st,
                                    stateid: None,
                                    change_info: None,
                                    result_flags: 0,
                                    delegation: None,
                                    attrset: vec![],
                                };
                            }
                        }
                    }
                } else if let Some(parent) = file_path.parent() {
                    if let Ok(pmd) = crate::nfs::v4::stat_cache::stat(parent) {
                        if let Err(st) = authz::check(
                            cred.as_ref(), &pmd, authz::W | authz::X, "OPEN(create)", parent,
                        ) {
                            return OpenRes {
                                status: st,
                                stateid: None,
                                change_info: None,
                                result_flags: 0,
                                delegation: None,
                                attrset: vec![],
                            };
                        }
                    }
                }
            }

            // change_info4 needs the parent's PRE-mutation change value,
            // sampled before the create so it equals what a client's
            // last GETATTR of this directory reported.
            let dir_before = file_path
                .parent()
                .and_then(crate::nfs::v4::change_counter::current_of_path)
                .unwrap_or(0);

            // Conflict sites 1+8 (design §5.2), create arm: an
            // UNCHECKED create over an EXISTING delegated file opens
            // it write-capable and may truncate it via createattrs-
            // size — the fence runs BEFORE the create executes. A
            // genuinely new file has no identity yet and nothing to
            // fence.
            let mut _deleg_guard = None;
            if crate::nfs::v4::state::delegations_enabled() {
                if let Some(ident) =
                    open_ident_of(crate::nfs::v4::stat_cache::stat(&file_path))
                {
                    let truncates = createattrs
                        .as_ref()
                        .map(|a| a.size.is_some())
                        .unwrap_or(false);
                    match self.state_mgr.deleg_fence(
                        ident,
                        self.fence_mutator(ctx),
                        truncates,
                        "open_create",
                    ) {
                        crate::nfs::v4::state::FenceVerdict::Proceed(g) => _deleg_guard = g,
                        crate::nfs::v4::state::FenceVerdict::Delay => {
                            info!("OPEN(create): delegation recall in flight → DELAY");
                            return OpenRes {
                                status: Nfs4Status::Delay,
                                stateid: None,
                                change_info: None,
                                result_flags: 0,
                                delegation: None,
                                attrset: vec![],
                            };
                        }
                    }
                }
            }

            // read+write: this fd is seeded into the fd-cache below and
            // must serve BOTH directions (a write-only fd turns a later
            // READ through the cache into EBADF).
            match crate::nfs::v4::open_beneath::open_async(
                tokio::fs::OpenOptions::new().read(true).write(true).create(true),
                &file_path,
            )
            .await
            {
                Ok(created) => {
                    debug!(
                        "OPEN: {} file {:?}",
                        if existed { "opened existing" } else { "created" },
                        file_path
                    );

                    // Blocker 2: OPEN(create) is a namespace operation
                    // when — and only when — it actually made a dirent.
                    // Opening a file that already existed changes no
                    // name, so it must not pay an fsync; this is the
                    // hot path for every read.
                    if !existed {
                        if let Err(e) =
                            crate::nfs::v4::metadata_sync::commit_parent_of(&file_path).await
                        {
                            warn!(
                                "OPEN(create): {:?} created but committing the parent \
                                 dirent failed: {} — refusing to ACK an operation that \
                                 is not durable",
                                file_path, e
                            );
                            return OpenRes {
                                status: Nfs4Status::Io,
                                stateid: None,
                                change_info: None,
                                result_flags: 0,
                                delegation: None,
                                attrset: vec![],
                            };
                        }
                    }

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
                                delegation: None,
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
                        // The birth is itself a mutation the tier must
                        // see. Without this note a file that is created
                        // and never written has no capture note, no
                        // dirty row and no generation row, so it is
                        // absent from the manifest — `touch .gitkeep`
                        // survives locally but does not exist in the
                        // bucket, and any restore silently loses it.
                        // Noted inside a write ticket so it cannot land
                        // in a swapped-out epoch (gate.rs's straggler
                        // invariant). A fresh inode cannot be under
                        // eviction, so the refusal arm is unreachable
                        // in practice — but if it ever fires, note
                        // anyway: a pessimal Whole beats a lost file.
                        match crate::tier::gate::enter_path(&file_path) {
                            Ok(_ticket) => crate::tier::capture::note_path(
                                &file_path,
                                crate::tier::capture::Mutation::Whole,
                            ),
                            Err(crate::tier::gate::Excluded) => {
                                warn!(
                                    "OPEN(create): write gate excluded a fresh file at {:?} — \
                                     noting dirty outside the ticket",
                                    file_path
                                );
                                crate::tier::capture::note_path(
                                    &file_path,
                                    crate::tier::capture::Mutation::Whole,
                                );
                            }
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
                                // Best effort by design — a chown failure
                                // must not fail the OPEN — but NOT silent.
                                // This used to discard the result entirely,
                                // so a server without CAP_CHOWN handed the
                                // client NFS4_OK for a file owned by the
                                // SERVER instead of the caller, with nothing
                                // logged anywhere. Measured: strip the
                                // capability and every file lands 65532:988
                                // instead of the caller's uid, silently.
                                // The wrong answer is worse than the refusal,
                                // so at minimum it must be visible.
                                if let Ok(Err(e)) = tokio::task::spawn_blocking(move || {
                                    std::os::unix::fs::chown(&p, want_uid, want_gid)
                                })
                                .await
                                {
                                    warn!(
                                        "OPEN(create): chown {:?} to {:?}:{:?} failed: {} — \
                                         the file is owned by the SERVER, not the caller. \
                                         Ownership-sensitive workloads will refuse to run. \
                                         Grant CAP_CHOWN (see the chart's securityContext).",
                                        file_path, want_uid, want_gid, e
                                    );
                                }
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
                            if self.stateid_quota_exhausted(client_id) {
                                return OpenRes {
                                    status: Nfs4Status::Delay,
                                    stateid: None,
                                    change_info: None,
                                    result_flags: 0,
                                    delegation: None,
                                    attrset: vec![],
                                };
                            }

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
                                    delegation: None,
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
                                // fstat the create's own fd — no path race.
                                open_ident_of(created.metadata()),
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
                                // Already a std File — `open_beneath`
                                // hands one back so both doors (sync
                                // and async) return the same type.
                                let file = Arc::new(created);
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
                                // REAL bracket, not the fabricated 0→1 it
                                // used to be: the client compares `before`
                                // to its cached directory change attr, and
                                // a mismatch invalidates the directory's
                                // dentry/access/attr caches — once per
                                // OPEN. Measured as one extra ACCESS RPC
                                // per created file (4021 vs knfsd's 8) and
                                // 3x the LOOKUPs on a delete storm. An
                                // OPEN of an EXISTING file made no dirent,
                                // so before == after and the client keeps
                                // its caches.
                                change_info: Some({
                                    let after = file_path
                                        .parent()
                                        .and_then(
                                            crate::nfs::v4::change_counter::current_of_path,
                                        )
                                        .unwrap_or_else(|| dir_before.wrapping_add(1));
                                    ChangeInfo {
                                        atomic: true,
                                        before: if existed { after } else { dir_before },
                                        after,
                                    }
                                }),
                                result_flags: OPEN4_RESULT_LOCKTYPE_POSIX,
                                // The CREATE arm never GRANTS (design §4
                                // rule 3: a just-created file has no warm
                                // re-access value, and skipping it removes
                                // a class of create/truncate races) — but
                                // it must still ANSWER a client that set a
                                // WANT bit. `claim_grantable: false` is
                                // what makes the refusal unconditional
                                // here; the want-bit arms sit above it in
                                // the rule order, so WANT_NO_DELEG still
                                // gets WND4_NOT_WANTED rather than a
                                // reason about the claim.
                                //
                                // Missing this was DELEG4's actual cause.
                                // The NONE_EXT encoder was already in
                                // place and correct; the create arm simply
                                // never asked, so pynfs kept reporting
                                // "Got no delegation, expected
                                // OPEN_DELEGATE_NONE_EXT" against a server
                                // that could produce one — and the test
                                // that creates a file is the one the RFC
                                // case is written around.
                                delegation: self.deleg_answer(
                                    ctx,
                                    client_id,
                                    &op,
                                    false,
                                    None,
                                    None,
                                    &[],
                                ),
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
                                delegation: None,
                                attrset: vec![],
                            };
                        }
                    }
                }
                Err(e) => {
                    warn!("OPEN: Failed to create file {:?}: {}", file_path, e);
                    let status = if crate::nfs::v4::open_beneath::is_symlink_refusal(&e) {
                        // The leaf became a symlink between the check
                        // above and this open — the race the pre-check
                        // cannot cover and O_NOFOLLOW does.
                        Nfs4Status::SymLink
                    } else {
                        match e.kind() {
                            std::io::ErrorKind::PermissionDenied => Nfs4Status::Access,
                            std::io::ErrorKind::AlreadyExists => Nfs4Status::Exist,
                            std::io::ErrorKind::NotFound => Nfs4Status::NoEnt,
                            _ => Nfs4Status::Io,
                        }
                    };
                    return OpenRes {
                        status,
                        stateid: None,
                        change_info: None,
                        result_flags: 0,
                        delegation: None,
                        attrset: vec![],
                    };
                }
            }
        }

        // OPEN without CREATE or CLAIM_FH - file must exist
        debug!("OPEN: Opening existing file (no create)");

        // Get client ID from session (set by SEQUENCE operation)
        let client_id = self.get_client_id_from_context(ctx);
        if self.stateid_quota_exhausted(client_id) {
            return OpenRes {
                status: Nfs4Status::Delay,
                stateid: None,
                change_info: None,
                result_flags: 0,
                delegation: None,
                attrset: vec![],
            };
        }

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
                // The cross-instance fallback, and it must be CONTAINED.
                //
                // This arm fires exactly when `resolve_handle` REFUSED
                // the handle — which is now also what it does to a
                // forged one, or one naming a path outside the export.
                // `parse_path_lenient` checks neither the tag nor the
                // instance (a DS has to honour MDS-minted handles), so
                // without this the OPEN path answered every refusal by
                // trying again with the parser that cannot refuse: the
                // path went to `seed_open_fd`, which opens it and
                // anchors the fd to the stateid for READ and WRITE to
                // find. A bypass of the containment check, reached by
                // sending a handle bad enough to fail validation.
                Err(_) => {
                    let embedded = match FileHandleManager::parse_path_lenient(current_fh) {
                        Ok(p) => match self.fh_mgr.contain(&p) {
                            Ok(contained) => Some(contained),
                            // Refusing is not the same as having no
                            // path: dropping it to None here would let
                            // the OPEN succeed anyway (this arm mints a
                            // stateid regardless), which is how the
                            // first version of this fix still let the
                            // escape through.
                            Err(e) => {
                                warn!(
                                    "OPEN(CLAIM_FH): handle names {:?}, which is not inside \
                                     the export ({}) — STALE",
                                    p, e
                                );
                                return OpenRes {
                                    status: Nfs4Status::Stale,
                                    stateid: None,
                                    change_info: None,
                                    result_flags: 0,
                                    delegation: None,
                                    attrset: vec![],
                                };
                            }
                        },
                        // No embedded path at all. Legitimate: v4 kernel
                        // handles carry only an ino. Unchanged.
                        Err(_) => None,
                    };
                    (parent_fh_data.clone(), embedded, false, None)
                }
            },
            // Normalized away at the top of the handler.
            OpenClaim::Previous { .. } | OpenClaim::DelegCur { .. } => {
                (parent_fh_data.clone(), None, false, None)
            }
        };

        // RFC 8881 §18.16.3, the no-create arm — and the one that
        // actually mattered. This path never opened anything itself; it
        // minted a stateid and left `seed_open_fd` (and, failing that,
        // READ's own fallback open) to resolve the path later. Both
        // followed the link, so a client could LOOKUP a symlink, OPEN
        // it, and READ whatever it pointed at. Refusing the OPEN is
        // both the RFC answer and the point at which the client is
        // still able to do the right thing.
        //
        // CLAIM_FH included: the filehandle names the link itself
        // (LOOKUP is required to return the link's own handle), so
        // arriving by handle is not evidence of anything.
        if let Some(p) = &target_path {
            if crate::nfs::v4::open_beneath::leaf_is_symlink(p) {
                warn!("OPEN(no-create): {:?} is a symlink → SYMLINK", p);
                return OpenRes {
                    status: Nfs4Status::SymLink,
                    stateid: None,
                    change_info: None,
                    result_flags: 0,
                    delegation: None,
                    attrset: vec![],
                };
            }
        }

        // ── §2A leg A3: the NO-CREATE OPEN path ─────────────────────
        //
        // There are TWO open paths, and the first version of this fix
        // patched only the create-capable one — so a plain
        // open-for-write of an existing file sailed straight past the
        // check while chown and chmod were correctly denied. That is the
        // §1.1 shape (a mechanism present on one path and absent from
        // the sibling) reproduced inside the fix for it, and it was
        // caught only because the drill kept failing on exactly one
        // assertion. Both paths check now.
        {
            use crate::nfs::v4::authz;
            if let Some(tp) = target_path.as_ref() {
              if let Ok(md) = crate::nfs::v4::stat_cache::stat(tp) {
                let mut want = 0u32;
                if op.share_access & 1 != 0 { want |= authz::R; }
                if op.share_access & 2 != 0 { want |= authz::W; }
                if want != 0 {
                    if let Err(st) = authz::check(
                        ctx.cred().as_ref(), &md, want, "OPEN(no-create)", tp,
                    ) {
                        return OpenRes {
                            status: st,
                            stateid: None,
                            change_info: None,
                            result_flags: 0,
                            delegation: None,
                            attrset: vec![],
                        };
                    }
                }
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
                delegation: None,
                attrset: vec![],
            };
        }

        // Served from the attr cache the OPEN compound already
        // warmed — not a fresh syscall on the smallfile hot path.
        let open_ident = target_path
            .as_deref()
            .and_then(|p| open_ident_of(crate::nfs::v4::stat_cache::stat(p)));

        // Conflict sites 1+2 (design §5.2): an OPEN with write access,
        // or one denying READ, on a delegated file recalls every
        // holder and answers DELAY. The consult runs BEFORE
        // open-state registration so a DELAYed conflictor leaves no
        // phantom open behind (the rollback rule — the original
        // register-first ordering left a write open visible to
        // share_conflict for the whole recall window, with no CLOSE
        // ever coming). The guard rides to the end of the handler:
        // grants refuse while the registration is in flight.
        let mut _deleg_guard = None;
        if (op.share_access & 2 != 0) || (op.share_deny & 1 != 0) {
            if let Some(ident) = open_ident {
                match self.state_mgr.deleg_fence(ident, Some(client_id), false, "open_write") {
                    crate::nfs::v4::state::FenceVerdict::Proceed(g) => _deleg_guard = g,
                    crate::nfs::v4::state::FenceVerdict::Delay => {
                        info!("OPEN(no-create): delegation recall in flight → DELAY");
                        return OpenRes {
                            status: Nfs4Status::Delay,
                            stateid: None,
                            change_info: None,
                            result_flags: 0,
                            delegation: None,
                            attrset: vec![],
                        };
                    }
                }
            }
        }

        // Record-or-bump the open (RFC 7530 §16.16: same (client,
        // owner, fh) gets the SAME stateid.other with seqid bumped,
        // share-mask merged).
        let fh_for_grant = target_fh_data.clone();
        let stateid = self.state_mgr.stateids.record_open(
            client_id,
            op.owner.clone(),
            target_fh_data,
            op.share_access,
            op.share_deny,
            None,
            open_ident,
        );

        debug!("OPEN: stateid {:?} for client {}", stateid, client_id);

        // F17c: anchor the open with an fd immediately.
        if let Some(p) = &target_path {
            self.seed_open_fd(&stateid, p, target_live);
        }

        // Design §4: the full grant rule set. Runs AFTER open-state
        // registration — the requester's own open is READ-only by
        // rule 4, so it never trips the write-open predicate — and
        // any refusal is free (NONE, never DELAY).
        let delegation = self.deleg_answer(
            ctx,
            client_id,
            &op,
            claim_grantable,
            open_ident,
            target_path.as_deref(),
            &fh_for_grant,
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
            // A no-create OPEN mutates nothing: before == after at the
            // directory's REAL change value, so the client keeps its
            // directory caches. The fabricated 0→1 this replaces forced
            // an invalidation on every open of an existing file.
            // CLAIM_FH opens carry no directory (target_path None); an
            // equal pair at 0 still never claims a change.
            change_info: Some({
                let cur = target_path
                    .as_deref()
                    .and_then(|p| p.parent())
                    .and_then(crate::nfs::v4::change_counter::current_of_path)
                    .unwrap_or(0);
                ChangeInfo { atomic: true, before: cur, after: cur }
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
    /// What the OPEN reply's delegation arm should say.
    ///
    /// `Silent` is not the same as `Explained(..)`: a client that set
    /// no WANT bit asked nothing, and OPEN_DELEGATE_NONE_EXT is also
    /// the signal that a server understands WANT bits at all, so
    /// volunteering it would answer a question that was never posed.
    /// With the feature flag off the answer is ALWAYS `Silent` — the
    /// kill switch's promise is that the wire looks exactly as it did
    /// before the feature existed, and an informational arm is still
    /// a wire change.
    fn deleg_answer(
        &self,
        ctx: &CompoundContext,
        client_id: u64,
        op: &OpenOp,
        claim_grantable: bool,
        ident: Option<(u64, u64)>,
        path: Option<&std::path::Path>,
        fh_bytes: &[u8],
    ) -> Option<crate::nfs::v4::compound::Delegation> {
        use crate::nfs::v4::compound::Delegation;
        let want = op.share_access & 0xFF00;
        match self.try_grant_read_delegation(
            ctx,
            client_id,
            op,
            claim_grantable,
            ident,
            path,
            fh_bytes,
        ) {
            Ok(stateid) => Some(Delegation::Read { stateid }),
            // The flag is off: say nothing new.
            Err(None) => None,
            Err(Some(why)) if want != 0 => Some(Delegation::NoneExt { why }),
            Err(Some(_)) => None,
        }
    }

    /// The grant rule set (design §4), in order, first failure wins.
    /// Every refusal answers no delegation and bumps its per-reason
    /// counter — delegations are optional and refusal must be free, so
    /// nothing here ever DELAYs.
    ///
    /// `Err(None)` means the feature is off, which is deliberately
    /// distinct from every other refusal: it is the one case where the
    /// server must not even admit the question was understood.
    fn try_grant_read_delegation(
        &self,
        _ctx: &CompoundContext,
        client_id: u64,
        op: &OpenOp,
        claim_grantable: bool,
        ident: Option<(u64, u64)>,
        path: Option<&std::path::Path>,
        fh_bytes: &[u8],
    ) -> Result<StateId, Option<WhyNoDelegation>> {
        let d = &self.state_mgr.delegations;

        // Rule 1 — gates. The env flag; the recall machinery (a
        // delegation the server cannot recall is the stale-forever
        // trap); the MDS posture (refused wholesale until slice 5
        // lands the write-capable-layout rule behind its own flag);
        // the circuit breaker (per-client quarantine first); the
        // sentinel kill-switch file.
        if !delegations_enabled() {
            return Err(None); // the default path — not a counted refusal
        }

        // Rule 4a — the client's OWN instruction, consulted before any
        // server-side gate. WANT_NO_DELEG and WANT_CANCEL are the
        // client telling us something rather than asking, and RFC 8881
        // §18.16.2 makes NOT_WANTED / CANCELLED the answer. Left below
        // the gates, a server that merely happened to be unable to
        // grant would answer WND4_RESOURCE to a client that asked for
        // no delegation — "I would have, but I could not", which is a
        // different statement and a false one.
        match op.share_access & 0xFF00 {
            0x0400 => {
                d.count_refusal("share_want");
                return Err(Some(WhyNoDelegation::NotWanted));
            }
            0x0500 => {
                d.count_refusal("share_want");
                return Err(Some(WhyNoDelegation::Cancelled));
            }
            _ => {}
        }

        if !self.state_mgr.recall_machinery_ready() {
            d.count_refusal("gate");
            return Err(Some(WhyNoDelegation::Resource));
        }
        // The MDS posture has its own flag (design §3, slice 5): a
        // layout holder's writes never cross the MDS, so granting
        // there rests on rule 6 and the LAYOUTGET/LAYOUTCOMMIT/proxy
        // fences, which get their own rig before a fleet grants.
        // Counted apart from "gate" so a rig against the MDS binary
        // can tell "posture refused" from "machinery missing".
        if self.state_mgr.pnfs_posture()
            && !crate::nfs::v4::state::pnfs_delegations_enabled()
        {
            d.count_refusal("posture");
            return Err(Some(WhyNoDelegation::Resource));
        }
        if d.grants_paused(client_id) {
            d.count_refusal("gate");
            return Err(Some(WhyNoDelegation::Resource));
        }
        if d.sentinel_blocked(self.fh_mgr.get_export_path()) {
            d.count_refusal("gate");
            return Err(Some(WhyNoDelegation::Resource));
        }

        // Rule 2 — grace, with the anything_reclaimable nuance: a
        // fresh-PVC / hibernate wake with nothing reclaimable does
        // NOT blackout grants for 90s. When grace is real, no grants
        // (a new grant could conflict with an unreclaimed
        // pre-restart write open).
        if self.state_mgr.leases.in_grace_period()
            && self.state_mgr.leases.anything_reclaimable()
        {
            d.count_refusal("grace");
            return Err(Some(WhyNoDelegation::Resource));
        }

        // Rule 3 — claim shape AS SENT: CLAIM_NULL / CLAIM_FH on the
        // no-create arm only (this fn is only called from it). The
        // create arm stays NONE on every path — a just-created file
        // has no warm re-access value, and skipping it removes a
        // class of create/truncate races.
        if !claim_grantable {
            d.count_refusal("claim");
            return Err(Some(WhyNoDelegation::Resource));
        }

        // Rule 4 — share bits, MASKED not compared: read-only access
        // and no deny. (The two want bits that refuse outright are
        // rule 4a above.) The old exact-match `== 1` would silently
        // refuse any client that sets want bits at all.
        if (op.share_access & 0x3) != 1 || op.share_deny != 0 {
            d.count_refusal("share_want");
            // The share mode itself makes a delegation unsafe, which
            // is contention by any honest reading.
            return Err(Some(WhyNoDelegation::Contention));
        }

        // Rule 7 — backchannel health: the recall path must exist for
        // THIS client before the server owes it a recall.
        if !self.state_mgr.callback_ready(client_id) {
            d.count_refusal("no_cb");
            return Err(Some(WhyNoDelegation::Resource));
        }

        // Rule 5 precondition — the file must be identifiable, or the
        // write-open predicate could silently answer "no writers"
        // about a file it cannot see.
        let (Some(ident), Some(path)) = (ident, path) else {
            d.count_refusal("no_ident");
            return Err(Some(WhyNoDelegation::Resource));
        };

        // Rule 6's key: the export-relative path the layout index is
        // consulted by. Outside the MDS posture the predicate is a
        // constant false and the key is never read.
        let file_key: String = path
            .strip_prefix(self.fh_mgr.get_export_path())
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned();

        // Rules 5, 6, 8, 9 — re-checked UNDER the file entry lock, with
        // the mint into the ONE stateid namespace (READ/TEST/FREE_
        // STATEID work on delegation stateids; no disjoint-namespace
        // BAD_STATEID trap). Unpersisted + epoch-mixed (design §6).
        match d.try_grant(
            crate::nfs::v4::state::FileId::new(ident.0, ident.1),
            client_id,
            fh_bytes.to_vec(),
            path.to_path_buf(),
            || {
                !self.state_mgr.stateids.file_has_write_open(ident.0, ident.1)
                    && !self.state_mgr.write_layout_held_by_other(&file_key, client_id)
            },
            || {
                self.state_mgr
                    .stateids
                    .allocate_delegation(client_id, fh_bytes.to_vec())
            },
        ) {
            Ok(sid) => {
                // DEBUG, not INFO: `try_grant` already logs this exact
                // event at INFO from the state layer, and two INFO
                // lines per grant is not just noise — a rig counting
                // "granted READ delegation" to prove grants happened
                // reads DOUBLE, which silently halves whatever
                // coverage floor it thought it was enforcing. One
                // event, one line.
                debug!(
                    "OPEN: delegation {:?} granted on {:?} to client {}",
                    sid, path, client_id
                );
                Ok(sid)
            }
            Err(refusal) => {
                debug!(
                    "OPEN: delegation refused for client {} on {:?}: {:?}",
                    client_id, path, refusal
                );
                // Everything `try_grant` refuses is another party's
                // hold on the file — a live write open, a conflicting
                // record, a revoked tombstone, the post-recall
                // cooldown — except the quotas, which are ours.
                Err(Some(refusal.why_no_delegation()))
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

        // Return the delegation. Error mapping is design §5.4: unknown
        // ⇒ BAD_STATEID, stale seqid ⇒ OLD_STATEID, revoked tombstone
        // ⇒ DELEG_REVOKED (retained until FREE_STATEID).
        match self.state_mgr.delegations.return_delegation(&stateid) {
            Ok(client_id) => {
                info!(
                    "DELEGRETURN: client {} returned delegation {:?}",
                    client_id, stateid
                );
                DelegReturnRes {
                    status: Nfs4Status::Ok,
                }
            }
            Err(e) => {
                warn!("DELEGRETURN: refused for {:?}: {:?}", stateid, e);
                DelegReturnRes {
                    status: match e {
                        crate::nfs::v4::state::DelegReturnError::Unknown => Nfs4Status::BadStateId,
                        crate::nfs::v4::state::DelegReturnError::OldSeqid => Nfs4Status::OldStateId,
                        crate::nfs::v4::state::DelegReturnError::Revoked => {
                            Nfs4Status::DelegRevoked
                        }
                    },
                }
            }
        }
    }

    /// Handle CLOSE operation
    pub fn handle_close(
        &self,
        op: CloseOp,
        ctx: &CompoundContext,
    ) -> CloseRes {
        debug!("CLOSE: stateid={:?}", op.stateid);

        // A stateid belongs to the client that established it. This took
        // none of that into account: the context was discarded outright
        // (`_ctx`), and `close_open` keys on `stateid.other` alone while
        // accepting `seqid == 0` as a wildcard, so not even a seqid had
        // to be guessed. `other` is `[global counter][client_id as u32]`
        // with no random component (`stateid.rs` `allocate`), so reaching
        // another client's open state is arithmetic rather than luck —
        // and destroying it is silent, because the victim learns nothing
        // until its next use of a stateid the server has already dropped.
        //
        // LOCK carried this guard; CLOSE and LOCKU did not.
        if let Some(client_id) = ctx
            .session_id
            .and_then(|sid| self.state_mgr.sessions.get_session(&sid).map(|s| s.client_id))
        {
            if self
                .state_mgr
                .stateids
                .belongs_to_other_client(&op.stateid, client_id)
            {
                warn!("CLOSE: stateid belongs to another client — refusing");
                return CloseRes {
                    status: Nfs4Status::BadStateId,
                    stateid: None,
                };
            }
        }

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
                    data: Bytes::new().into(),
                };
            }
        };

        // Validate stateid with relaxed checking for READ operations
        // This allows seqid=0 for anonymous/first reads
        if let Err(e) = self.state_mgr.stateids.validate_for_read(&op.stateid) {
            warn!("READ: Invalid stateid: {}", e);
            return ReadRes {
                status: self.state_mgr.stateids.invalid_status(&op.stateid),
                eof: false,
                data: Bytes::new().into(),
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
                        data: Bytes::new().into(),
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
                    return ReadRes { status: Nfs4Status::Delay, eof: false, data: Bytes::new().into() };
                }
            }
        }

        // Get filename for logging before moving path
        let filename = path.file_name().map(|n| n.to_string_lossy().to_string());
        // Step 11: kept for the evicted-DELAY arm's RPC parking (only
        // cloned when the tier is on; the hot path pays one branch).
        let hyd_park_path = crate::tier::capture::enabled().then(|| path.clone());

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
        // `count` is a raw wire u32 (up to 4 GiB) that sizes the read
        // buffer below, otherwise bounded only by FILE size — and
        // multi-GB files are this product's headline content, so a
        // ~100-byte frame declaring count=0xFFFFFFFF forced a
        // gigabyte-scale allocation. The session's response cap
        // (REP_TOO_BIG) is enforced only AFTER encoding — after the
        // allocation. Clamp to the server's response ceiling first: a
        // short READ is legal (the client resumes from eof=false), no
        // negotiated session cap exceeds this constant, and a reply
        // above it could never be sent anyway.
        let count = (op.count as usize)
            .min(super::session::MAX_IO_PAYLOAD as usize);
        let fd_cache = Arc::clone(&self.fd_cache);
        let stateid_other = op.stateid.other;
        // Splice staging is opt-in per COMPOUND and default off. It also
        // requires that no slot is caching this reply: the cache must
        // store the exact octets a replay returns (RFC 8881 15.1.10.4),
        // which a payload that never enters userspace cannot supply.
        // SEQUENCE is op 0, so `cache_slot` is already decided here.
        let may_splice = ctx.can_splice && ctx.cache_slot.is_none();

        // Kept out of the closure's move: the success arm forgets the
        // path's attr-cache entry (the read moved its atime).
        let read_path = path.clone();

        let read_job =
            move || -> std::io::Result<(crate::nfs::segment::Segment, bool)> {
            let file = match cached {
                Some(f) => f,
                None => {
                    // Prefer read+write so a later WRITE on this
                    // stateid reuses the entry; fall back to
                    // read-only when the file mode denies write.
                    use crate::nfs::v4::open_beneath;
                    let (file, writable) = match open_beneath::open(
                        std::fs::OpenOptions::new().read(true).write(true),
                        &path,
                    ) {
                        Ok(f) => (f, true),
                        // A symlink leaf is refused in BOTH arms — the
                        // read-only retry must not become the way in.
                        Err(e) if open_beneath::is_symlink_refusal(&e) => return Err(e),
                        Err(_) => (open_beneath::open_read(&path)?, false),
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
            // Sampled BEFORE the consult: the post-read guard requires
            // the marker cycle unchanged across the whole read window
            // (FlintTierMarker's CycleBlind counterexample — a COMPLETE
            // evict+hydrate cycle inside the window clears the marker
            // before the re-consult looks).
            #[cfg(unix)]
            let marker_cycle_began = crate::tier::evict::marker_cycle();
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if crate::tier::evict::is_evicted(metadata.dev(), metadata.ino()) {
                    // Step 11: this READ is the hydration trigger.
                    // A Blocked verdict means no eviction can ever make
                    // room for this object, so DELAY would be a promise
                    // the volume cannot keep — the client would retry
                    // until something killed it.
                    if let crate::tier::hydrate::Verdict::Blocked(size) =
                        crate::tier::hydrate::request(
                            metadata.dev(),
                            metadata.ino(),
                            &path,
                            crate::tier::hydrate::Trigger::Read,
                        )
                    {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::StorageFull,
                            format!("tier: {size} bytes cannot fit this volume"),
                        ));
                    }
                    crate::tier::meter::bump(crate::tier::meter::Counter::EvictedOpDelays);
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WouldBlock,
                        "tier: file evicted (awaiting hydration)",
                    ));
                }
            }
            let file_size = metadata.len();

            // A read at or past EOF answers empty+eof BEFORE the tier
            // re-consult below — preserved from before the fast path
            // moved into `read_pool::read_segment`.
            if offset >= file_size {
                return Ok((Bytes::new().into(), true));
            }

            // The shared clamp → splice → pooled-read fast path. On the
            // splice arm nothing is on the wire yet, which is what lets
            // the tier re-consult below still retract the payload.
            let (payload, eof) = crate::nfs::read_pool::read_segment(
                &file, file_size, offset, count, may_splice,
            )?;

            // Re-consult AFTER the read (review finding (b), caught
            // live by the chaos drill's evict/hydrate churn: git once
            // read an empty .git/config). READs are deliberately
            // lock-free, so an eviction can land between the consult
            // above and the pread — the pread then sees the truncated
            // stub and would serve a short/empty result as if it were
            // file content. C2's marker-BEFORE-truncate order makes
            // this re-check airtight against the eviction; the CYCLE
            // half of the guard covers what the marker alone cannot —
            // a complete evict+hydrate cycle inside the window whose
            // completed hydration already CLEARED the marker
            // (FlintTierMarker's find) — answer DELAY; the retry
            // serves the restored bytes.
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if !crate::tier::evict::read_window_intact(
                    metadata.dev(),
                    metadata.ino(),
                    marker_cycle_began,
                ) {
                    if let crate::tier::hydrate::Verdict::Blocked(size) =
                        crate::tier::hydrate::request(
                            metadata.dev(),
                            metadata.ino(),
                            &path,
                            crate::tier::hydrate::Trigger::Read,
                        )
                    {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::StorageFull,
                            format!("tier: {size} bytes cannot fit this volume"),
                        ));
                    }
                    crate::tier::meter::bump(crate::tier::meter::Counter::EvictedOpDelays);
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WouldBlock,
                        "tier: file evicted mid-read (awaiting hydration)",
                    ));
                }
            }

            // NOTE for the splice path: every error return between the
            // stage above and this line DROPS `payload`, and dropping a
            // staged payload retracts it — the pipe is destroyed and not
            // one byte reaches the client. That is exactly what the tier
            // re-consult needs, and it is why staging goes to a pipe
            // rather than straight to the socket.
            Ok((payload, eof))
        };
        // See `read_mode`. Default is SpawnBlocking.
        let read_result = match read_mode() {
            ReadMode::Inline => Ok(read_job()),
            ReadMode::BlockInPlace => Ok(tokio::task::block_in_place(read_job)),
            ReadMode::SpawnBlocking => tokio::task::spawn_blocking(read_job).await,
        };

        match read_result {
            Ok(Ok((data, eof))) => {
                debug!("READ: Read {} bytes at offset {} from {:?}, eof={}",
                      data.len(), op.offset, filename.as_deref().unwrap_or("unknown"), eof);
                // The read moved the file's atime, which no counter
                // bump tracks (change must NOT advance on reads — it
                // would invalidate every client's data cache). Drop
                // the attr-cache entry so the next GETATTR sees the
                // new atime instead of serving it stale for a TTL.
                crate::nfs::v4::stat_cache::forget(&read_path);
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
                    // An object this volume can never hold. NOSPC is
                    // terminal on purpose: DELAY would be a promise to
                    // serve bytes that will never fit.
                    std::io::ErrorKind::StorageFull => Nfs4Status::NoSpc,
                    _ => Nfs4Status::Io,
                };
                // Step 11: park the RPC up to the hold bound before
                // answering DELAY — one DELAY per hold instead of ten
                // per second (step-9 finding 1), and the client's next
                // retry serves ~0.1 s after the restore lands.
                if status == Nfs4Status::Delay {
                    if let Some(p) = &hyd_park_path {
                        crate::tier::hydrate::park(p).await;
                    }
                }
                ReadRes {
                    status,
                    eof: false,
                    data: Bytes::new().into(),
                }
            }
            Err(e) => {
                warn!("READ: Task spawn error: {}", e);
                ReadRes {
                    status: Nfs4Status::Io,
                    eof: false,
                    data: Bytes::new().into(),
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

        // A READ-delegation stateid on WRITE is an access-mode violation
        // (RFC 8881 §18.32.3): the delegation conveys read rights only,
        // and RFC requires OPENMODE, not a recall of the writer's own
        // delegation followed by a write under a stateid naming no open.
        if self.state_mgr.stateids.is_delegation(&op.stateid) {
            warn!("WRITE: READ-delegation stateid presented for write -> OPENMODE");
            return WriteRes {
                status: Nfs4Status::OpenMode,
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

        // Conflict site 6 (design §5.2): WRITE is the mandatory
        // backstop — anonymous/special stateids bypass OPEN entirely,
        // so the OPEN fence alone cannot protect a delegated file
        // from writes. Recall + DELAY like any other mutation lane.
        // The stat is behind the flag: with delegations off this whole
        // block is one atomic load.
        let mut _deleg_guard = None;
        if crate::nfs::v4::state::delegations_enabled() {
            if let Some(ident) = open_ident_of(crate::nfs::v4::stat_cache::stat(&path)) {
                match self
                    .state_mgr
                    .deleg_fence(ident, self.fence_mutator(ctx), false, "write")
                {
                    crate::nfs::v4::state::FenceVerdict::Proceed(g) => _deleg_guard = g,
                    crate::nfs::v4::state::FenceVerdict::Delay => {
                        info!("WRITE: delegation recall in flight → DELAY");
                        return WriteRes {
                            status: Nfs4Status::Delay,
                            count: 0,
                            committed: UNSTABLE4,
                            writeverf: 0,
                        };
                    }
                }
            }
        }

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
                crate::nfs::v4::open_beneath::open(
                    std::fs::OpenOptions::new().read(true).write(true).create(true),
                    &path_clone,
                )
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
        // Step 11: the evicted-DELAY arm's parking handle.
        let hyd_park_path = cap_path.clone();

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

        let write_job = move || -> std::io::Result<usize> {
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
                        // Step 11: hydrate-first WRITE barrier — this
                        // write's trigger carries WRITE priority
                        // (step-9 finding 2: bound the fsync park).
                        if let Some(p) = cap_path.as_deref() {
                            // The write barrier hydrates first, so an
                            // object that can never land here can never
                            // be written through either — NOSPC now
                            // beats a DELAY the volume cannot honour.
                            if let crate::tier::hydrate::Verdict::Blocked(size) =
                                crate::tier::hydrate::request(
                                    md.dev(),
                                    md.ino(),
                                    p,
                                    crate::tier::hydrate::Trigger::Write,
                                )
                            {
                                return Err(std::io::Error::new(
                                    std::io::ErrorKind::StorageFull,
                                    format!("tier: {size} bytes cannot fit this volume"),
                                ));
                            }
                        }
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

            // F14 + attr cache: the data is READ-VISIBLE the moment
            // pwrite returns — the counter must advance NOW, not after
            // the sync below (FILE_SYNC's fsync is milliseconds, and a
            // GETATTR in that window answering from a counter-validated
            // cache entry would pair the new bytes with the old size:
            // the beyond-EOF disease at fsync scale). The change attr
            // tracks mutation visibility, not durability.
            if let Ok(md) = file_arc.metadata() {
                use std::os::unix::fs::MetadataExt;
                crate::nfs::v4::change_counter::bump(
                    md.dev(),
                    md.ino(),
                    crate::nfs::v4::change_counter::ctime_ns(&md),
                );
            }

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
        };
        // See `write_mode`: the handoff is skipped only for UNSTABLE
        // writes; anything carrying an fsync keeps the blocking pool.
        let write_result = if stable == UNSTABLE4 {
            match write_mode() {
                WriteMode::BlockInPlace => Ok(tokio::task::block_in_place(write_job)),
                WriteMode::SpawnBlocking => tokio::task::spawn_blocking(write_job).await,
            }
        } else {
            tokio::task::spawn_blocking(write_job).await
        };

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
                    // An object this volume can never hold. NOSPC is
                    // terminal on purpose: DELAY would be a promise to
                    // serve bytes that will never fit.
                    std::io::ErrorKind::StorageFull => Nfs4Status::NoSpc,
                    _ => Nfs4Status::Io,
                });
                // Step 11: park before the DELAY (see the READ arm).
                if status == Nfs4Status::Delay {
                    if let Some(p) = &hyd_park_path {
                        crate::tier::hydrate::park(p).await;
                    }
                }
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
                let file = crate::nfs::v4::open_beneath::open(
                    std::fs::OpenOptions::new().write(true),
                    &path,
                )?;
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
        let excl = crate::tier::gate::exclude(dev, ino)
            .expect("exclusion must be available on an idle file");
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

    /// change_info4 must carry the directory's REAL change bracket —
    /// the values GETATTR serves — not fabricated constants. The client
    /// compares `before` with its cached directory change attribute; a
    /// mismatch invalidates the directory's dentry/access/attr caches,
    /// which the fabricated 0→1 forced on EVERY OPEN (measured: one
    /// extra ACCESS RPC per created file — 4021 vs knfsd's 8 — and 3x
    /// the LOOKUPs on a delete storm). Falsified against the old code:
    /// before read 0 and after 1 regardless of the directory.
    #[tokio::test]
    async fn open_change_info_is_the_directorys_real_change_bracket() {
        let (handler, fh_mgr, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);
        let export_fh = fh_mgr.path_to_filehandle(fh_mgr.get_export_path()).unwrap();
        ctx.current_fh = Some(export_fh.clone());
        let open = |name: &str| OpenOp {
            seqid: 0,
            share_access: OPEN4_SHARE_ACCESS_BOTH,
            share_deny: OPEN4_SHARE_DENY_NONE,
            owner: b"cinfo-owner".to_vec(),
            openhow: OpenHow::Create(Fattr4 { attrmask: vec![], attr_vals: vec![] }),
            claim: OpenClaim::Null(name.to_string()),
        };
        let dir = fh_mgr.get_export_path().to_path_buf();

        let cached = crate::nfs::v4::change_counter::current_of_path(&dir).unwrap();
        let res = handler.handle_open(open("cinfo-a.bin"), &mut ctx).await;
        assert_eq!(res.status, Nfs4Status::Ok);
        let ci = res.change_info.expect("OPEN(create) must carry change_info");
        assert!(ci.atomic);
        assert_eq!(
            ci.before, cached,
            "before must equal the value a client's last GETATTR cached"
        );
        let now = crate::nfs::v4::change_counter::current_of_path(&dir).unwrap();
        assert_eq!(ci.after, now, "after must equal what GETATTR now serves");
        assert_ne!(ci.after, ci.before, "a create moved the directory");

        // Re-open the SAME file: no dirent was made, so the bracket
        // must report no change — what lets the client keep its
        // directory caches on every open of an existing file.
        ctx.current_fh = Some(export_fh.clone());
        let res2 = handler.handle_open(open("cinfo-a.bin"), &mut ctx).await;
        assert_eq!(res2.status, Nfs4Status::Ok);
        let ci2 = res2.change_info.expect("change_info on an existing-open too");
        assert_eq!(ci2.before, ci2.after, "no dirent made ⇒ before == after");
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
        // Plants a marker, which bumps the PROCESS-GLOBAL MARKER_CYCLE.
        // Any concurrent test with an open read window sees it broken —
        // no shared file needed. Held for the whole body.
        let _excl = crate::tier::capture::test_exclusive();
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

    /// Step 11 end-to-end: a READ of an evicted file triggers
    /// hydration through the real handler; the parked retry serves the
    /// restored bytes. The ONLY test that installs the global
    /// hydrator (module drills use local instances — a second global
    /// install would race this one).
    #[tokio::test]
    async fn read_of_evicted_file_hydrates_and_serves() {
        // Queues and/or drains the PROCESS-GLOBAL capture queue.
        // Held for the whole body: the theft window is queue-to-drain,
        // not the drain alone. See `capture::test_exclusive`.
        let _excl = crate::tier::capture::test_exclusive();
        use crate::tier::{capture, evict, flush, hydrate, store::memory::MemoryStore};
        use std::os::unix::fs::MetadataExt;
        capture::force_enable();
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
                    owner: b"hydrate-owner".to_vec(),
                    openhow: OpenHow::Create(Fattr4 { attrmask: vec![], attr_vals: vec![] }),
                    claim: OpenClaim::Null("hydrate-e2e.bin".to_string()),
                },
                &mut ctx,
            )
            .await;
        assert_eq!(open_res.status, Nfs4Status::Ok);
        let stateid = open_res.stateid.unwrap();
        let path = fh_mgr.get_export_path().join("hydrate-e2e.bin");
        let content = b"the bytes that went to the bucket and back".to_vec();
        std::fs::write(&path, &content).unwrap();
        let md = std::fs::metadata(&path).unwrap();
        let (dev, ino) = (md.dev(), md.ino());
        capture::forget(dev, ino);

        // Publish + evict through the real tier machinery.
        let mem = std::sync::Arc::new(MemoryStore::new());
        let store: std::sync::Arc<dyn crate::tier::store::ObjectStore> = mem.clone();
        let backend: std::sync::Arc<dyn crate::state_backend::StateBackend> =
            std::sync::Arc::new(crate::state_backend::memory::MemoryBackend::new());
        let mut fcfg =
            flush::FlushConfig::new(fh_mgr.get_export_path().to_path_buf(), "t/".into());
        fcfg.floor = std::time::Duration::ZERO;
        fcfg.quiesce = std::time::Duration::ZERO;
        let orch = flush::FlushOrchestrator::new(
            store.clone(),
            backend.clone(),
            fcfg,
            crate::tier::epoch::EpochGuard::held(1),
        );
        capture::note_path(&path, capture::Mutation::Whole);
        for _ in 0..50 {
            let _ = crate::tier::durable::drain_pending(&backend).await;
            if backend
                .tier_list_dirty()
                .await
                .unwrap()
                .iter()
                .any(|r| r.dev == dev && r.ino == ino && r.path.is_some())
            {
                break;
            }
            capture::clear_durable(dev, ino);
            capture::note_path(&path, capture::Mutation::Whole);
        }
        orch.tick().await;
        let g = orch.generation_of(dev, ino).expect("must publish");
        let out = evict::evict_file(&backend, &store, &path, &g.key, &|_, _| false).await;
        assert!(matches!(out, evict::EvictOutcome::Evicted { .. }), "{:?}", out);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);

        hydrate::install(
            backend.clone(),
            store,
            hydrate::HydrateConfig {
                hold: std::time::Duration::from_secs(5),
                concurrency: 2,
                ..Default::default()
            },
        );

        // First READ: triggers hydration, parks, answers DELAY.
        let r1 = handler
            .handle_read(
                ReadOp { stateid: stateid.clone(), offset: 0, count: content.len() as u32 },
                &ctx,
            )
            .await;
        assert_eq!(r1.status, Nfs4Status::Delay, "the triggering READ answers DELAY");
        // The park should have outlived the (instant) restore; the
        // retry — the kernel's 0.1 s clock — serves.
        let r2 = handler
            .handle_read(
                ReadOp { stateid, offset: 0, count: content.len() as u32 },
                &ctx,
            )
            .await;
        assert_eq!(r2.status, Nfs4Status::Ok, "the retry must serve the restored bytes");
        assert_eq!(r2.data.as_mem().as_ref(), content.as_slice(), "byte-identical after the round trip");
        assert!(!evict::is_evicted(dev, ino));
        assert!(
            capture::snapshot(dev, ino).is_none_or(|c| !c.is_dirty()),
            "hydration must not dirty the file"
        );
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

    /// The write verifier's two obligations, pinned CLOCK-INDEPENDENTLY.
    ///
    /// Across incarnations: the shipped mint was wall-clock SECONDS, so
    /// any two server processes started within one second shared a
    /// verifier — a client holding uncommitted UNSTABLE data matched
    /// COMMIT against the impostor, dropped its dirty pages, and never
    /// re-sent. A tight construction loop lands every handler in the
    /// same second (mostly the same millisecond), so under that mint
    /// this test fails by construction rather than by racing a second
    /// boundary — two handlers straddling a tick would pass vacuously.
    ///
    /// Within one incarnation: WRITE, COMMIT and COPY must report ONE
    /// value for the life of the process — a changing verifier is the
    /// documented Linux 6.8 COPY+COMMIT infinite resend loop.
    #[test]
    fn write_verifier_differs_across_incarnations_and_holds_within_one() {
        let mut seen = std::collections::HashSet::new();
        let mut handlers = Vec::new();
        for i in 0..64 {
            let (h, _fh, t) = create_test_handler();
            assert!(
                seen.insert(h.write_verifier()),
                "incarnation {i} re-minted an earlier verifier — a same-second \
                 restart would silently discard clients' uncommitted UNSTABLE data"
            );
            handlers.push((h, t));
        }
        let (h, _t) = &handlers[0];
        assert_eq!(
            h.write_verifier(),
            h.write_verifier(),
            "the verifier must be constant within one incarnation"
        );
    }

    /// B6, per-client stateid quota: OPEN's mint arms refuse with DELAY
    /// at the cap — stateids were mintable without bound by any
    /// unauthenticated peer, each one memory plus a persisted row.
    #[tokio::test]
    async fn open_refuses_with_delay_at_the_stateid_quota() {
        use crate::nfs::v4::state::StateQuotas;
        let q = StateQuotas {
            max_clients: 4096,
            max_sessions_per_client: 16,
            max_stateids_per_client: 1,
            max_locks_per_client: 65536,
        };
        let temp = TempDir::new().unwrap();
        std::fs::write(temp.path().join("f.txt"), b"data").unwrap();
        let fh_mgr = Arc::new(FileHandleManager::new(temp.path().to_path_buf()));
        let state_mgr = Arc::new(StateManager::new_in_memory_with_quotas("", q));
        let handler = IoOperationHandler::new(Arc::clone(&state_mgr), fh_mgr.clone());
        // The client already holds its one allowed stateid.
        let _held = state_mgr.stateids.allocate(StateType::Open, 1, None);

        let mut ctx = CompoundContext::new(0);
        ctx.current_fh = Some(fh_mgr.path_to_filehandle(&temp.path().join("f.txt")).unwrap());
        let res = handler
            .handle_open(
                OpenOp {
                    seqid: 0,
                    share_access: OPEN4_SHARE_ACCESS_READ,
                    share_deny: OPEN4_SHARE_DENY_NONE,
                    owner: b"quota-owner".to_vec(),
                    openhow: OpenHow::NoCreate,
                    claim: OpenClaim::Fh,
                },
                &mut ctx,
            )
            .await;
        assert_eq!(
            res.status,
            Nfs4Status::Delay,
            "an OPEN past max_stateids_per_client must be refused with DELAY \
             (CLOSE and lease expiry free capacity)"
        );
    }

    /// A READ `count` is attacker-chosen up to 4 GiB and used to size a
    /// heap allocation bounded otherwise only by file size; the
    /// response-size gate (REP_TOO_BIG) runs after encoding — after the
    /// allocation. Against a file larger than the response ceiling, a
    /// count=0xFFFFFFFF READ must come back clamped to the ceiling
    /// (short reads are legal; the client resumes from eof=false),
    /// never sized to the file. Observed red pre-clamp: 4 MiB returned
    /// for one ~100-byte request frame.
    #[tokio::test]
    async fn read_count_is_clamped_to_the_response_ceiling() {
        use crate::nfs::v4::operations::session::MAX_IO_PAYLOAD;
        // Hold the tier rig lock: MARKER_CYCLE is process-global and
        // every marker INSERT bumps it, so a concurrent planter breaks
        // this test's read window. See the block below for the measured
        // mechanism.
        let _excl = crate::tier::capture::test_exclusive();
        let (handler, fh_mgr, _temp) = create_test_handler();
        let mut ctx = CompoundContext::new(0);

        // A file 4x larger than the response ceiling.
        let big_path = fh_mgr.get_export_path().join("big.bin");
        let f = std::fs::File::create(&big_path).unwrap();
        f.set_len(4 * MAX_IO_PAYLOAD as u64).unwrap();
        drop(f);

        // WAS A FLAKE — green alone, red in the full suite. CLOSED by
        // giving every marker-planting test the rig lock this one
        // already held; the cause is recorded here because the obvious
        // reading of it is wrong.
        //
        // A failure here reports `Delay != Ok`, which comes from the
        // tier consult and has nothing to do with the response ceiling
        // the test is named for. The consult that fires is the POST-read
        // window guard, `read_window_intact`, and it is a conjunction:
        //
        //     !is_evicted(dev, ino) && marker_cycle() == began
        //
        // The recorded diagnosis for two months was `(dev, ino)`
        // ALIASING — a tier test's marker landing on this TempDir file's
        // reused inode. Instrumenting the failure refuted it:
        //
        //     capture_on=true  marker_on_this_file=false  cycle 1->2
        //
        // No marker on this file, no aliasing, no shared inode. It is
        // the SECOND conjunct: MARKER_CYCLE is a single process-global
        // counter bumped by every marker insert anywhere, so ANY other
        // test planting ANY marker inside this read window breaks it.
        // Nothing about the two tests need be related.
        //
        // Two process-global statics have to line up for it, which is
        // why it needed the suite and never reproduced alone:
        //
        //   1. `capture::enable()` is STICKY — deliberately no disable
        //      (a tier that turned off mid-run would strand queued
        //      marks). One tier test's `force_enable()` therefore leaves
        //      the consult live for every test that follows it, in a
        //      process where the tier is otherwise off.
        //   2. `is_evicted` is gated on `capture::enabled()`;
        //      `marker_cycle()` is not. So half the guard is tier-gated
        //      and half is unconditional.
        //
        // The product behaviour is deliberate and stays: a global
        // counter means an unrelated file's eviction costs one spurious
        // DELAY retry, which `evict.rs` weighs explicitly against
        // per-identity narrowing (evictions are rare; a warm fill's
        // clears must not storm). Production evicts rarely; a test
        // binary evicts constantly. So the fix is test isolation, not a
        // product change — every cycle-bumping test now takes
        // `capture::test_exclusive()`, which this test already held.
        //
        // The clear below stays: it removes any marker left behind
        // BEFORE this test, which the lock cannot undo.
        {
            use std::os::unix::fs::MetadataExt;
            let md = std::fs::metadata(&big_path).unwrap();
            crate::tier::evict::forget(md.dev(), md.ino());
            assert!(
                crate::tier::evict::logical_size(md.dev(), md.ino()).is_none(),
                "precondition: this file must carry no eviction marker"
            );
        }

        ctx.current_fh = Some(fh_mgr.path_to_filehandle(&big_path).unwrap());

        let open_res = handler
            .handle_open(
                OpenOp {
                    seqid: 0,
                    share_access: OPEN4_SHARE_ACCESS_READ,
                    share_deny: OPEN4_SHARE_DENY_NONE,
                    owner: b"amp-owner".to_vec(),
                    openhow: OpenHow::NoCreate,
                    claim: OpenClaim::Fh,
                },
                &mut ctx,
            )
            .await;
        let stateid = open_res.stateid.unwrap();

        let cycle_at_open = crate::tier::evict::marker_cycle();
        let res = handler
            .handle_read(ReadOp { stateid, offset: 0, count: u32::MAX }, &ctx)
            .await;
        // Self-diagnosing: if the rig lock is ever dropped from a
        // planter, this names the cause instead of reporting a bare
        // `Delay != Ok` that points at the response ceiling.
        assert_eq!(
            res.status,
            Nfs4Status::Ok,
            "tier consult fired: capture={} marker_on_this_file={} marker_cycle moved by {} \
             during the read window (a non-zero delta means some test planted a marker \
             without holding capture::test_exclusive())",
            crate::tier::capture::enabled(),
            {
                use std::os::unix::fs::MetadataExt;
                let md = std::fs::metadata(&big_path).unwrap();
                crate::tier::evict::logical_size(md.dev(), md.ino()).is_some()
            },
            crate::tier::evict::marker_cycle() - cycle_at_open,
        );
        let data = res.data;
        assert!(
            data.len() <= MAX_IO_PAYLOAD as usize,
            "count=0xFFFFFFFF returned {} bytes — the allocation tracked the FILE, \
             not the payload ceiling (the ~100-byte-frame-to-gigabytes amplification). \
             Asserted against MAX_IO_PAYLOAD, not the advertised channel cap: the \
             cap now sits 2 KiB above it, and a clamp that used the cap would slip \
             through an assertion written against it.",
            data.len()
        );
        assert!(!data.is_empty(), "the clamp must not turn the read into an empty reply");
        assert!(!res.eof, "a clamped read mid-file must report eof=false so the client resumes");
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
        assert_eq!(res.delegation, None, "gate off ⇒ OPEN_DELEGATE_NONE");
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

    /// CLOSE used to discard its CompoundContext outright (`_ctx`) and
    /// key `close_open` on `stateid.other` alone, accepting `seqid == 0`
    /// as a wildcard. `other` is `[global counter][client_id as u32]`
    /// with no random component, so reaching another client's open state
    /// was arithmetic, and destroying it was silent.
    ///
    /// Three arms, because a bare "the foreign CLOSE is refused" oracle
    /// would pass against a CLOSE that refused everything:
    ///   1. the foreign CLOSE is refused,
    ///   2. the state SURVIVES it, and
    ///   3. the rightful owner can still close.
    #[tokio::test]
    async fn close_refuses_a_stateid_belonging_to_another_client() {
        let (handler, fh_mgr, _temp) = create_test_handler();

        let mk_session = |cid: u64| {
            handler.state_mgr.sessions.create_session(
                cid, 0, 0, 1024 * 1024, 1024 * 1024, 64 * 1024, 8, 8, 0, None, 1,
            ).session_id
        };

        let mut owner_ctx = CompoundContext::new(1);
        owner_ctx.current_fh = Some(fh_mgr.get_root_fh().unwrap());
        owner_ctx.session_id = Some(mk_session(11));

        let open_res = handler.handle_open(OpenOp {
            seqid: 0,
            share_access: OPEN4_SHARE_ACCESS_READ,
            share_deny: OPEN4_SHARE_DENY_NONE,
            owner: b"victim".to_vec(),
            openhow: OpenHow::NoCreate,
            claim: OpenClaim::Fh,
        }, &mut owner_ctx).await;
        assert_eq!(open_res.status, Nfs4Status::Ok);
        let victim_stateid = open_res.stateid.unwrap();

        // A different client, presenting the victim's stateid — and with
        // seqid 0, the wildcard that needed no guessing at all.
        let mut attacker_ctx = CompoundContext::new(1);
        attacker_ctx.current_fh = Some(fh_mgr.get_root_fh().unwrap());
        attacker_ctx.session_id = Some(mk_session(22));

        let refused = handler.handle_close(
            CloseOp { seqid: 0, stateid: StateId { seqid: 0, other: victim_stateid.other } },
            &attacker_ctx,
        );
        assert_eq!(
            refused.status, Nfs4Status::BadStateId,
            "a foreign client must not be able to CLOSE this state"
        );

        // The state is still there — the refusal did not half-destroy it.
        assert!(
            handler.state_mgr.stateids.validate(&victim_stateid).is_ok(),
            "the victim's open state must survive the refused CLOSE"
        );

        // And the owner is unaffected. Without this arm the leg would
        // also pass against a CLOSE that refused every caller.
        let owner_close = handler.handle_close(
            CloseOp { seqid: 0, stateid: victim_stateid },
            &owner_ctx,
        );
        assert_eq!(
            owner_close.status, Nfs4Status::Ok,
            "the owning client must still be able to close"
        );
    }

    /// The OPEN path answered a REFUSED handle by trying again with the
    /// parser that cannot refuse.
    ///
    /// `resolve_handle` checks the tag, the instance and containment.
    /// `parse_path_lenient` checks none of them by design — a pNFS Data
    /// Server has to honour handles the Metadata Server minted, whose
    /// instance and key are not its own. CLAIM_FH used the second as the
    /// fallback for the first, so a handle bad enough to be REJECTED got
    /// its embedded path opened by `seed_open_fd` and anchored to the
    /// stateid for READ and WRITE to find.
    ///
    /// The control is the point: the same forged-tag handle naming a
    /// path INSIDE the export must still open, because that is the
    /// cross-instance case the fallback exists for. Only containment may
    /// refuse here — not the bad tag, or this leg would pass against a
    /// server that had simply deleted the fallback.
    #[tokio::test]
    async fn open_by_handle_cannot_escape_the_export_through_the_lenient_parser() {
        let (handler, fh_mgr, _temp) = create_test_handler();

        // A v1 handle with a GARBAGE tag, so `resolve_handle` refuses it
        // and the lenient fallback is what answers — exactly the attack.
        let forge = |path: &std::path::Path| {
            let p = path.to_str().unwrap();
            let mut data = vec![1u8];
            data.extend_from_slice(&0u64.to_be_bytes());
            data.extend_from_slice(&[0xABu8; 32]);
            data.extend_from_slice(&(p.len() as u16).to_be_bytes());
            data.extend_from_slice(p.as_bytes());
            crate::nfs::v4::protocol::Nfs4FileHandle { data }
        };
        let open_op = || OpenOp {
            seqid: 0,
            share_access: OPEN4_SHARE_ACCESS_READ,
            share_deny: OPEN4_SHARE_DENY_NONE,
            owner: b"claim-fh".to_vec(),
            openhow: OpenHow::NoCreate,
            claim: OpenClaim::Fh,
        };

        // CONTROL: in-export, same unverifiable tag. Must succeed.
        let inside = fh_mgr.get_export_path().join("testfile.txt");
        assert!(inside.exists(), "the rig's fixture file must be there");
        let mut ok_ctx = CompoundContext::new(1);
        ok_ctx.current_fh = Some(forge(&inside));
        let allowed = handler.handle_open(open_op(), &mut ok_ctx).await;
        assert_eq!(
            allowed.status, Nfs4Status::Ok,
            "the cross-instance fallback must still work for an in-export path"
        );

        // THE ESCAPE: a real file outside the export.
        let outside_dir = TempDir::new().unwrap();
        let secret = outside_dir.path().join("state.db");
        std::fs::write(&secret, b"not yours").unwrap();
        let mut bad_ctx = CompoundContext::new(1);
        bad_ctx.current_fh = Some(forge(&secret));
        let refused = handler.handle_open(open_op(), &mut bad_ctx).await;
        assert_ne!(
            refused.status, Nfs4Status::Ok,
            "OPEN must not resolve a handle naming a path outside the export"
        );

        // And nothing was anchored: no fd may have been seeded for it,
        // or a later READ would reach the file through the cache even
        // though the OPEN said no.
        if let Some(sid) = refused.stateid {
            assert!(
                handler.fd_cache.get(&sid.other).is_none(),
                "a refused OPEN must leave no anchored fd behind"
            );
        }
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
        assert_eq!(read_res.data.as_mem().as_ref(), b"test content");
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
        assert_eq!(read_res.data.as_mem().as_ref(), b"Hello, NFS!");

        // 4. CLOSE
        let close_op = CloseOp {
            seqid: 0,
            stateid,
        };

        let close_res = handler.handle_close(close_op, &ctx);
        assert_eq!(close_res.status, Nfs4Status::Ok);
    }

    /// The WANT bits, and the two things that must stay separate:
    /// what the server DID (no delegation) and whether it says WHY.
    ///
    /// OPEN_DELEGATE_NONE_EXT is not just an explanation — it is also
    /// how a server tells a client it understands WANT bits at all
    /// (RFC 8881 §18.16.2/3). So it is sent only to a client that
    /// actually set one, and NEVER with the feature flag off: the kill
    /// switch's promise is that the wire looks exactly as it did before
    /// delegations existed, and an informational arm is still a change
    /// on the wire.
    #[test]
    fn the_none_ext_arm_answers_want_bits_and_only_with_the_flag_on() {
        use crate::nfs::v4::compound::Delegation;
        const WANT_READ: u32 = 0x0100;
        const WANT_NO_DELEG: u32 = 0x0400;
        const WANT_CANCEL: u32 = 0x0500;

        let (handler, fh_mgr, _temp) = create_test_handler();
        let ctx = CompoundContext::new(0);
        let root = fh_mgr.get_root_fh().unwrap();
        let ask = |handler: &IoOperationHandler, want: u32| {
            let op = OpenOp {
                seqid: 0,
                share_access: OPEN4_SHARE_ACCESS_READ | want,
                share_deny: 0,
                owner: b"want-owner".to_vec(),
                openhow: OpenHow::NoCreate,
                claim: OpenClaim::Fh,
            };
            handler.deleg_answer(
                &ctx,
                1,
                &op,
                true,
                Some((1, 1)),
                Some(std::path::Path::new("/x")),
                root.data.as_slice(),
            )
        };

        // ── flag OFF: the dark-behavior pin. Even a client that set
        // WANT_NO_DELEG — the one case the RFC makes mandatory for a
        // want-bit-aware server — gets the plain NONE arm, because a
        // server with delegations switched off is not one.
        {
            let _g = crate::nfs::v4::state::with_delegations(false);
            assert!(
                ask(&handler, WANT_NO_DELEG).is_none(),
                "the kill switch must not leak a NONE_EXT arm",
            );
            assert!(ask(&handler, WANT_READ).is_none());
        }

        // ── flag ON. No callback path is wired in this harness, so
        // every grant is refused at rule 7 — which is exactly the
        // point: the ANSWER differs by what the client asked, not by
        // what the server decided.
        let _g = crate::nfs::v4::state::with_delegations(true);
        assert!(
            !handler.state_mgr.recall_machinery_ready(),
            "precondition: this harness cannot grant, so every leg below \
             is a refusal and the want bits are the only variable",
        );

        assert_eq!(
            ask(&handler, WANT_NO_DELEG),
            Some(Delegation::NoneExt { why: WhyNoDelegation::NotWanted }),
        );
        assert_eq!(
            ask(&handler, WANT_CANCEL),
            Some(Delegation::NoneExt { why: WhyNoDelegation::Cancelled }),
        );
        // Asked for one and could not have it: the reason is the
        // server's own state, not the client's request.
        assert_eq!(
            ask(&handler, WANT_READ),
            Some(Delegation::NoneExt { why: WhyNoDelegation::Resource }),
        );
        // Asked nothing, so is told nothing. Same refusal underneath.
        assert!(
            ask(&handler, 0).is_none(),
            "a client that set no WANT bit is owed no explanation",
        );
    }

    #[test]
    fn read_only_open_grants_no_delegation_by_default() {
        // Delegations are gated off (FLINT_NFS_DELEGATIONS unset): a
        // conflict-free read-only open of an existing file — the case the
        // grant path fires on — must yield OPEN_DELEGATE_NONE and mint no
        // server-side delegation record. Granting without a working
        // CB_RECALL path would let a client cache stale data forever.
        // The gate is process-global and cargo runs tests in parallel,
        // so asserting the DEFAULT requires excluding every test that
        // forces it on. Without this the assertion is a coin flip that
        // depends on which other tests the filter happened to select.
        let _excl = crate::nfs::v4::state::deleg_flag_exclusive();
        let (handler, fh_mgr, _temp) = create_test_handler();

        assert!(!delegations_enabled());
        let ctx = CompoundContext::new(0);
        let op = OpenOp {
            seqid: 0,
            share_access: OPEN4_SHARE_ACCESS_READ,
            share_deny: 0,
            owner: b"pin-owner".to_vec(),
            openhow: OpenHow::NoCreate,
            claim: OpenClaim::Fh,
        };
        let delegation = handler.deleg_answer(
            &ctx,
            1,
            &op,
            true,
            Some((1, 1)),
            Some(std::path::Path::new("/x")),
            fh_mgr.get_root_fh().unwrap().data.as_slice(),
        );
        assert!(delegation.is_none());
        assert_eq!(handler.state_mgr.delegations.live_count(), 0);
    }

    /// The escape, run end to end as a client would run it.
    ///
    /// Every step here is a legal NFS operation: CREATE a symlink (the
    /// server is required to support it), LOOKUP it (the server is
    /// required to return the LINK's own filehandle, not the target's),
    /// then OPEN and READ. The server's job is to stop at the OPEN with
    /// NFS4ERR_SYMLINK so the client READLINKs and resolves the target
    /// in its OWN namespace — where `/data/state/state.db` means the
    /// client's file, not the hub's.
    ///
    /// Before this guard the server followed the link on the client's
    /// behalf, and since the hub's namespace holds its state database,
    /// its service-account token and its S3 credentials, "read any file
    /// the server process can read" is the whole security boundary.
    #[tokio::test]
    async fn opening_a_symlink_never_dereferences_it() {
        let (handler, fh_mgr, temp) = create_test_handler();

        // The target sits OUTSIDE the export — as the hub's state db and
        // its projected service-account token both do.
        let outside = temp.path().parent().unwrap().join("flint-secret-target");
        std::fs::write(&outside, b"AKIA-hub-credentials").unwrap();
        let link = temp.path().join("innocent.txt");
        std::os::unix::fs::symlink(&outside, &link).unwrap();

        let export_fh = fh_mgr.path_to_filehandle(fh_mgr.get_export_path()).unwrap();
        let mut ctx = CompoundContext::new(0);
        ctx.current_fh = Some(export_fh.clone());

        // CLAIM_NULL, no create: LOOKUP-then-OPEN, the ordinary shape.
        let res = handler
            .handle_open(
                OpenOp {
                    seqid: 0,
                    share_access: OPEN4_SHARE_ACCESS_READ,
                    share_deny: OPEN4_SHARE_DENY_NONE,
                    owner: b"attacker".to_vec(),
                    openhow: OpenHow::NoCreate,
                    claim: OpenClaim::Null("innocent.txt".to_string()),
                },
                &mut ctx,
            )
            .await;
        assert_eq!(
            res.status,
            Nfs4Status::SymLink,
            "OPEN of a symlink must answer NFS4ERR_SYMLINK (RFC 8881 §18.16.3)"
        );
        assert!(res.stateid.is_none(), "a refused OPEN must mint no stateid");

        // CLAIM_FH: arriving with the link already as CFH is not
        // evidence of anything — LOOKUP is REQUIRED to hand out the
        // link's own handle, so this is the same request by another road.
        let mut ctx = CompoundContext::new(0);
        ctx.current_fh = Some(fh_mgr.path_to_filehandle(&link).unwrap());
        let res = handler
            .handle_open(
                OpenOp {
                    seqid: 0,
                    share_access: OPEN4_SHARE_ACCESS_READ,
                    share_deny: OPEN4_SHARE_DENY_NONE,
                    owner: b"attacker".to_vec(),
                    openhow: OpenHow::NoCreate,
                    claim: OpenClaim::Fh,
                },
                &mut ctx,
            )
            .await;
        assert_eq!(res.status, Nfs4Status::SymLink, "CLAIM_FH is the same hole");

        // And the create form, which additionally used to TRUNCATE the
        // target on the way in: O_CREAT without O_EXCL follows a link.
        let mut ctx = CompoundContext::new(0);
        ctx.current_fh = Some(export_fh);
        let res = handler
            .handle_open(
                OpenOp {
                    seqid: 0,
                    share_access: OPEN4_SHARE_ACCESS_BOTH,
                    share_deny: OPEN4_SHARE_DENY_NONE,
                    owner: b"attacker".to_vec(),
                    openhow: OpenHow::Create(Fattr4 { attrmask: vec![], attr_vals: vec![] }),
                    claim: OpenClaim::Null("innocent.txt".to_string()),
                },
                &mut ctx,
            )
            .await;
        assert_eq!(res.status, Nfs4Status::SymLink, "OPEN(CREATE) must refuse too");

        // The target was never read and never written.
        assert_eq!(
            std::fs::read(&outside).unwrap(),
            b"AKIA-hub-credentials",
            "the file outside the export must be untouched"
        );
        let _ = std::fs::remove_file(&outside);
    }

    /// The no-create OPEN path never opened anything itself — it minted
    /// a stateid and left the resolution to READ's own fallback open. So
    /// refusing at OPEN is necessary but not sufficient: READ must also
    /// refuse, because a stateid from an earlier legitimate OPEN can
    /// outlive the file being replaced by a link, and because READ
    /// accepts the anonymous stateid without any OPEN at all.
    #[tokio::test]
    async fn reading_through_a_symlink_filehandle_is_refused() {
        let (handler, fh_mgr, temp) = create_test_handler();

        let outside = temp.path().parent().unwrap().join("flint-secret-read-target");
        std::fs::write(&outside, b"super secret bytes").unwrap();
        let link = temp.path().join("link.dat");
        std::os::unix::fs::symlink(&outside, &link).unwrap();

        let mut ctx = CompoundContext::new(0);
        ctx.current_fh = Some(fh_mgr.path_to_filehandle(&link).unwrap());

        let res = handler
            .handle_read(
                ReadOp { stateid: StateId::ANONYMOUS, offset: 0, count: 4096 },
                &ctx,
            )
            .await;
        assert_ne!(res.status, Nfs4Status::Ok, "READ must not serve a symlink's target");
        assert!(res.data.is_empty(), "and must return no bytes of it");
        let _ = std::fs::remove_file(&outside);
    }
}
