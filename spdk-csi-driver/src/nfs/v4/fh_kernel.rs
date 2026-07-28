//! v4 kernel inode filehandles — the F26 §12 target architecture.
//!
//! A v4 handle wraps the kernel's opaque `name_to_handle_at(2)` handle
//! (ext4: 32-bit ino + 32-bit generation) instead of embedding a path.
//! Resolution is `open_by_handle_at(2)` + a `/proc/self/fd` readlink —
//! no path maps, no bookkeeping on RENAME/REMOVE, rename-stability and
//! generation-staleness come from the kernel (retires the F17/F23/F26
//! mechanism class). See docs/f26-filehandle-cache-redesign.md §12.
//!
//! ## Wire format (≈46 B on ext4, ≤128 B NFS4 limit)
//!
//! `[ver=4][instance_id:8][ino:8][hmac:16][handle_type:4][klen:1][khandle:N]`
//!
//! * `instance_id` sits at the same offset as v1/v2/v3 so
//!   `validate_handle`'s instance check works unchanged.
//! * `ino` is flint's own portable object identity — the F17b/c
//!   unlink-open fallbacks key the open-files view by it (a kernel
//!   handle for an unlinked-but-open inode answers ESTALE, so the fd
//!   anchored at OPEN is the only way to keep serving it).
//! * `hmac` = HMAC-SHA256 truncated to 16 B over
//!   `instance_id‖ino‖handle_type‖khandle`, keyed by a per-volume
//!   secret at `<export>/.flint-nfs/fh.key`. Kernel handles are small
//!   enumerable values and `open_by_handle_at` bypasses directory
//!   permissions — the tag restores the unforgeability that v3's
//!   embedded SHA-256 provided (§12.1b). The key lives on the export
//!   volume so handles stay valid across pod failover; a per-boot key
//!   would re-introduce STALE-on-restart.
//!
//! ## Privilege
//!
//! `open_by_handle_at` needs `CAP_DAC_READ_SEARCH` (spiked 2026-07-19:
//! the minimal grant suffices, root not required — file capabilities
//! on the binary + the cap in the pod's bounding set). `KernelFh::
//! try_new` probes a real mint/resolve roundtrip at startup; on any
//! failure the caller falls back to path-based handles with a loud
//! warning instead of serving all-STALE (the mis-deployed-
//! securityContext safety net).

use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::path::{Path, PathBuf};

use super::protocol::Nfs4FileHandle;

type HmacSha256 = Hmac<Sha256>;

pub const FH_V4_VERSION: u8 = 4;
const HMAC_LEN: usize = 16;
/// Fixed part: ver(1) + instance(8) + ino(8) + hmac(16) + htype(4) + klen(1).
pub const FH_V4_MIN: usize = 38;
/// Kernel handles are ≤ MAX_HANDLE_SZ(128) but real filesystems use
/// 8–28 bytes; cap what we'll embed so the wire handle stays small.
const MAX_KHANDLE: usize = 64;

/// The object inode recorded in a v4 handle (bytes 9..17). Advisory —
/// used only to key server-side open-file fallbacks; resolution
/// authority is the HMAC-verified kernel handle.
pub fn v4_ino(data: &[u8]) -> Option<u64> {
    if data.first() == Some(&FH_V4_VERSION) && data.len() >= FH_V4_MIN {
        let mut b = [0u8; 8];
        b.copy_from_slice(&data[9..17]);
        Some(u64::from_be_bytes(b))
    } else {
        None
    }
}

fn mac_tag(key: &[u8; 32], instance_id: u64, ino: u64, htype: i32, kh: &[u8]) -> [u8; HMAC_LEN] {
    let mut mac = HmacSha256::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(&instance_id.to_be_bytes());
    mac.update(&ino.to_be_bytes());
    mac.update(&htype.to_be_bytes());
    mac.update(kh);
    let full = mac.finalize().into_bytes();
    let mut tag = [0u8; HMAC_LEN];
    tag.copy_from_slice(&full[..HMAC_LEN]);
    tag
}

/// Assemble a v4 wire handle. Pure — unit-tested on every platform.
pub fn encode_v4(
    key: &[u8; 32],
    instance_id: u64,
    ino: u64,
    handle_type: i32,
    khandle: &[u8],
) -> Result<Nfs4FileHandle, String> {
    if khandle.len() > MAX_KHANDLE {
        return Err(format!("kernel handle too large: {} bytes", khandle.len()));
    }
    let mut data = Vec::with_capacity(FH_V4_MIN + khandle.len());
    data.push(FH_V4_VERSION);
    data.extend_from_slice(&instance_id.to_be_bytes());
    data.extend_from_slice(&ino.to_be_bytes());
    data.extend_from_slice(&mac_tag(key, instance_id, ino, handle_type, khandle));
    data.extend_from_slice(&handle_type.to_be_bytes());
    data.push(khandle.len() as u8);
    data.extend_from_slice(khandle);
    Ok(Nfs4FileHandle { data })
}

/// Parse + authenticate a v4 wire handle → (ino, handle_type, khandle).
/// Rejects tampered/truncated/foreign-instance handles.
pub fn decode_v4(
    key: &[u8; 32],
    expect_instance: u64,
    data: &[u8],
) -> Result<(u64, i32, Vec<u8>), String> {
    if data.first() != Some(&FH_V4_VERSION) || data.len() < FH_V4_MIN {
        return Err("not a v4 filehandle".to_string());
    }
    let mut b8 = [0u8; 8];
    b8.copy_from_slice(&data[1..9]);
    let instance = u64::from_be_bytes(b8);
    if instance != expect_instance {
        return Err(format!(
            "stale v4 handle: instance {} != {}",
            instance, expect_instance
        ));
    }
    b8.copy_from_slice(&data[9..17]);
    let ino = u64::from_be_bytes(b8);
    let mut tag = [0u8; HMAC_LEN];
    tag.copy_from_slice(&data[17..33]);
    let mut b4 = [0u8; 4];
    b4.copy_from_slice(&data[33..37]);
    let htype = i32::from_be_bytes(b4);
    let klen = data[37] as usize;
    if data.len() != FH_V4_MIN + klen {
        return Err("v4 filehandle length mismatch".to_string());
    }
    let khandle = &data[FH_V4_MIN..];
    // Constant-time-enough for our threat model (forged handles from
    // NFS clients); a timing oracle on 16 bytes over TCP RTTs is not
    // practical, and hmac's Mac::verify_slice would need re-MACing
    // anyway, which is what we do.
    if mac_tag(key, instance, ino, htype, khandle) != tag {
        return Err("v4 filehandle authentication failed".to_string());
    }
    Ok((ino, htype, khandle.to_vec()))
}

/// Load (or first-boot create, 0600) the per-volume handle-auth key at
/// `<export>/.flint-nfs/fh.key`. Travels with the volume: handles stay
/// valid across pod failover.
pub fn load_or_create_key(export_root: &Path) -> Result<[u8; 32], String> {
    let dir = export_root.join(".flint-nfs");
    let path = dir.join("fh.key");
    match std::fs::read(&path) {
        Ok(bytes) if bytes.len() == 32 => {
            let mut k = [0u8; 32];
            k.copy_from_slice(&bytes);
            return Ok(k);
        }
        Ok(bytes) => {
            return Err(format!("fh.key corrupt: {} bytes (want 32)", bytes.len()));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("fh.key read: {}", e)),
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir .flint-nfs: {}", e))?;
    let key: [u8; 32] = rand::random();
    // Write-then-rename so a crash mid-write can't leave a short key.
    let tmp = dir.join(".fh.key.tmp");
    std::fs::write(&tmp, key).map_err(|e| format!("fh.key write: {}", e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, &path).map_err(|e| format!("fh.key rename: {}", e))?;
    Ok(key)
}

/// Why a mint failed — pre-existence mints (`NoEnt`) fall back to the
/// legacy v1 path handle at the call site.
#[derive(Debug)]
pub enum MintError {
    NoEnt,
    Other(String),
}

/// Why a resolve failed — `Stale` maps to NFS4ERR_STALE; the F17b/c
/// call-site fallbacks then try the ino-keyed open-files view.
#[derive(Debug)]
pub enum ResolveError {
    Stale,
    Other(String),
}

// ---------------------------------------------------------------------------
// F52: inode-identity path recovery.
//
// `open_by_handle_at` on a COLD dcache (a freshly staged mount — exactly
// what a cross-node server relocation produces) succeeds but materializes
// the inode as a DISCONNECTED dentry, and the kernel's name for such a
// dentry is literally "/". The old resolve trusted the
// `/proc/self/fd` readlink unconditionally, so every fh-only op resolved
// to "/": WRITE opened the container root (EISDIR → NFS4ERR_IO → client
// EIO → postgres PANIC on fdatasync) and GETATTR served the root
// directory's attributes with NFS4_OK (type/fileid flip → client-side
// ESTALE), with zero server-side errors. See
// docs/f52-estale-on-rwx-server-relocation.md §4 and the repro in its
// evidence bundle.
//
// The recovery below never trusts a resolved path unless it (a) sits
// under the export root and (b) lstats back to the fd's own (dev,ino).
// Anything else is re-located by inode identity with a bounded walk of
// the export tree — which also reconnects dentries as it goes, so one
// walk heals the whole storm. A walk that finds nothing answers Stale
// (visible, and the F17b/c open-file fallbacks still get their turn) —
// never a foreign path.
// ---------------------------------------------------------------------------

/// Inode identity on one filesystem: (st_dev, st_ino).
#[cfg(any(target_os = "linux", test))]
pub(crate) type InoKey = (u64, u64);

/// Default cap on the identity index (entries). Tunable via
/// FLINT_FH_IDENT_MAX; 0 disables the index (and the startup prewarm) —
/// recovery then always uses targeted early-exit walks.
#[cfg(any(target_os = "linux", test))]
const IDENT_CAP_DEFAULT: usize = 200_000;
/// Entry budget for a targeted (early-exit) walk.
#[cfg(any(target_os = "linux", test))]
const TARGETED_BUDGET: usize = 2_000_000;
/// A COMPLETE index younger than this is authoritative for misses — a
/// truly-gone inode must not trigger a re-walk per probe (unlink storms).
#[cfg(any(target_os = "linux", test))]
const REWALK_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(1);

#[cfg(any(target_os = "linux", test))]
pub(crate) fn ident_cap_from_env() -> usize {
    std::env::var("FLINT_FH_IDENT_MAX")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(IDENT_CAP_DEFAULT)
}

/// A resolved path is trustworthy iff it lies under the export root AND
/// still names the fd's inode. "/" (the disconnected-dentry name), a
/// "(deleted)"-suffixed path, or a since-renamed path all fail here.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn trusted_resolution(path: &Path, export_root: &Path, dev: u64, ino: u64) -> bool {
    use std::os::unix::fs::MetadataExt;
    path.starts_with(export_root)
        && std::fs::symlink_metadata(path)
            .map(|m| m.dev() == dev && m.ino() == ino)
            .unwrap_or(false)
}

/// Bounded breadth walk of `root` building (dev,ino) → path for every
/// entry on `dev`. Returns (index, complete). Side effect that matters
/// as much as the result: every lstat CONNECTS the entry's dentry, so a
/// walk warms the dcache for direct `/proc/self/fd` resolution too.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn walk_ino_index(
    root: &Path,
    dev: u64,
    cap: usize,
) -> (std::collections::HashMap<InoKey, PathBuf>, bool) {
    use std::os::unix::fs::MetadataExt;
    let mut map = std::collections::HashMap::new();
    if let Ok(md) = std::fs::symlink_metadata(root) {
        map.insert((md.dev(), md.ino()), root.to_path_buf());
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for ent in rd.flatten() {
            let Ok(md) = ent.metadata() else { continue };
            if md.dev() != dev {
                continue; // never index across (or descend into) a foreign mount
            }
            if map.len() >= cap {
                return (map, false);
            }
            let p = ent.path();
            map.insert((md.dev(), md.ino()), p.clone());
            if md.is_dir() {
                stack.push(p);
            }
        }
    }
    (map, true)
}

/// Targeted early-exit variant: find ONE inode, touch at most `budget`
/// entries. For trees too large to index.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn find_by_ino(root: &Path, key: InoKey, budget: usize) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    if let Ok(md) = std::fs::symlink_metadata(root) {
        if (md.dev(), md.ino()) == key {
            return Some(root.to_path_buf());
        }
    }
    let mut seen = 0usize;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for ent in rd.flatten() {
            let Ok(md) = ent.metadata() else { continue };
            if md.dev() != key.0 {
                continue;
            }
            let p = ent.path();
            if (md.dev(), md.ino()) == key {
                return Some(p);
            }
            seen += 1;
            if seen >= budget {
                return None;
            }
            if md.is_dir() {
                stack.push(p);
            }
        }
    }
    None
}

#[cfg(any(target_os = "linux", test))]
struct IdentState {
    map: std::collections::HashMap<InoKey, PathBuf>,
    complete: bool,
    built_at: Option<std::time::Instant>,
}

/// Serialized inode→path recovery over one export tree. One walker at a
/// time; concurrent cold resolves (the relocation replay storm) queue on
/// the lock and are all served by the first walk's index.
#[cfg(any(target_os = "linux", test))]
pub(crate) struct IdentityResolver {
    root: PathBuf,
    dev: u64,
    cap: usize,
    state: std::sync::Mutex<IdentState>,
    #[cfg(test)]
    pub(crate) walks: std::sync::atomic::AtomicUsize,
}

#[cfg(any(target_os = "linux", test))]
impl IdentityResolver {
    pub(crate) fn new(root: PathBuf, dev: u64, cap: usize) -> Self {
        Self {
            root,
            dev,
            cap,
            state: std::sync::Mutex::new(IdentState {
                map: Default::default(),
                complete: false,
                built_at: None,
            }),
            #[cfg(test)]
            walks: Default::default(),
        }
    }

    fn bump(&self) {
        #[cfg(test)]
        self.walks.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Build the index up front (server startup, before the listener
    /// accepts) so relocation replay never hits a cold path at all.
    /// Returns (entries, complete); no-op in cap=0 (targeted-only) mode.
    pub(crate) fn prewarm(&self) -> (usize, bool) {
        if self.cap == 0 {
            return (0, false);
        }
        self.bump();
        let (map, complete) = walk_ino_index(&self.root, self.dev, self.cap);
        let n = map.len();
        let mut st = self.state.lock().unwrap();
        st.map = map;
        st.complete = complete;
        st.built_at = Some(std::time::Instant::now());
        (n, complete)
    }

    /// The current path of (dev,ino), or None if it has no path under
    /// the export root (unlinked, or budget exhausted — both answer
    /// Stale upstream, engaging the F17b/c open-fd fallbacks).
    pub(crate) fn locate(&self, dev: u64, ino: u64) -> Option<PathBuf> {
        use std::os::unix::fs::MetadataExt;
        if dev != self.dev {
            return None;
        }
        let key = (dev, ino);
        if self.cap == 0 {
            self.bump();
            return find_by_ino(&self.root, key, TARGETED_BUDGET);
        }
        let mut st = self.state.lock().unwrap();
        let hit_stale = match st.map.get(&key) {
            Some(p) => {
                if std::fs::symlink_metadata(p)
                    .map(|m| m.dev() == dev && m.ino() == ino)
                    .unwrap_or(false)
                {
                    return Some(p.clone());
                }
                true // the index lies (renamed/removed since the walk) — rebuild NOW
            }
            None => false,
        };
        // A miss against a fresh COMPLETE index is authoritative; a
        // stale HIT is not (a rename inside the cooldown must re-walk,
        // or postgres's write-temp-then-rename pattern would STALE).
        if !hit_stale
            && st.complete
            && st.built_at.map_or(false, |t| t.elapsed() < REWALK_COOLDOWN)
        {
            return None;
        }
        self.bump();
        let (map, complete) = walk_ino_index(&self.root, self.dev, self.cap);
        st.map = map;
        st.complete = complete;
        st.built_at = Some(std::time::Instant::now());
        match st.map.get(&key) {
            Some(p) => Some(p.clone()),
            None if !complete => {
                self.bump();
                find_by_ino(&self.root, key, TARGETED_BUDGET)
            }
            None => None,
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::*;
    use std::os::unix::ffi::OsStrExt;

    const MAX_HANDLE_SZ: usize = 128;

    /// Matches the kernel's `struct file_handle` (variable-length
    /// f_handle; we always allocate the max).
    #[repr(C)]
    struct FileHandleBuf {
        handle_bytes: u32,
        handle_type: i32,
        f_handle: [u8; MAX_HANDLE_SZ],
    }

    /// Kernel-handle backend: an O_PATH fd on the export root (the
    /// `mount_fd` for `open_by_handle_at`), the per-volume HMAC key,
    /// and the instance id stamped into every handle. `identity` is the
    /// F52 recovery: resolutions that fall outside the export root
    /// (disconnected dentries name themselves "/") are re-located by
    /// (dev,ino) instead of being served as-is.
    pub struct KernelFh {
        mount_fd: i32,
        key: [u8; 32],
        instance_id: u64,
        export_root: PathBuf,
        identity: super::IdentityResolver,
    }

    // A RawFd and plain data — safe to share across threads.
    unsafe impl Send for KernelFh {}
    unsafe impl Sync for KernelFh {}

    impl Drop for KernelFh {
        fn drop(&mut self) {
            unsafe { libc::close(self.mount_fd) };
        }
    }

    fn cpath(p: &Path) -> Result<std::ffi::CString, String> {
        std::ffi::CString::new(p.as_os_str().as_bytes()).map_err(|_| "NUL in path".to_string())
    }

    impl KernelFh {
        /// Build the backend and PROBE it: mint + resolve a real object
        /// (`fh.key` itself) end-to-end. Any failure — missing
        /// CAP_DAC_READ_SEARCH, seccomp, unsupported fs — surfaces here
        /// so the caller can fall back to path handles loudly.
        pub fn try_new(export_root: &Path, instance_id: u64) -> Result<Self, String> {
            let key = load_or_create_key(export_root)?;
            let c = cpath(export_root)?;
            // O_RDONLY, NOT O_PATH: open_by_handle_at's mount_fd lookup
            // rejects O_PATH descriptors (EBADF — hit live on 6.1; the
            // resolved object fd below may still be O_PATH).
            let fd = unsafe {
                libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY)
            };
            if fd < 0 {
                return Err(format!(
                    "open export root O_PATH: {}",
                    std::io::Error::last_os_error()
                ));
            }
            // The export fs's device id anchors inode identity (F52):
            // (st_dev, st_ino) is a file's identity on one filesystem.
            let mut st: libc::stat = unsafe { std::mem::zeroed() };
            if unsafe { libc::fstat(fd, &mut st) } != 0 {
                let e = std::io::Error::last_os_error();
                unsafe { libc::close(fd) };
                return Err(format!("fstat export root: {}", e));
            }
            let export_root = export_root.to_path_buf();
            let this = Self {
                mount_fd: fd,
                key,
                instance_id,
                identity: super::IdentityResolver::new(
                    export_root.clone(),
                    st.st_dev as u64,
                    super::ident_cap_from_env(),
                ),
                export_root,
            };
            let probe_obj = this.export_root.join(".flint-nfs").join("fh.key");
            let fh = match this.mint(&probe_obj) {
                Ok(fh) => fh,
                Err(MintError::NoEnt) => return Err("probe object missing".to_string()),
                Err(MintError::Other(e)) => return Err(format!("probe mint: {}", e)),
            };
            match this.resolve(&fh.data) {
                Ok(_) => Ok(this),
                Err(ResolveError::Stale) => Err("probe resolve answered stale".to_string()),
                Err(ResolveError::Other(e)) => Err(format!("probe resolve: {}", e)),
            }
        }

        /// Mint a v4 handle for an existing object.
        pub fn mint(&self, path: &Path) -> Result<Nfs4FileHandle, MintError> {
            let c = cpath(path).map_err(MintError::Other)?;
            let mut buf = FileHandleBuf {
                handle_bytes: MAX_HANDLE_SZ as u32,
                handle_type: 0,
                f_handle: [0u8; MAX_HANDLE_SZ],
            };
            let mut mount_id: i32 = 0;
            let rc = unsafe {
                libc::syscall(
                    libc::SYS_name_to_handle_at,
                    libc::AT_FDCWD,
                    c.as_ptr(),
                    &mut buf as *mut FileHandleBuf,
                    &mut mount_id as *mut i32,
                    0,
                )
            };
            if rc != 0 {
                let err = std::io::Error::last_os_error();
                return match err.raw_os_error() {
                    Some(libc::ENOENT) => Err(MintError::NoEnt),
                    _ => Err(MintError::Other(format!("name_to_handle_at: {}", err))),
                };
            }
            let ino = std::fs::symlink_metadata(path)
                .map(|m| std::os::unix::fs::MetadataExt::ino(&m))
                .map_err(|e| MintError::Other(format!("stat after mint: {}", e)))?;
            let kh = &buf.f_handle[..buf.handle_bytes as usize];
            encode_v4(&self.key, self.instance_id, ino, buf.handle_type, kh)
                .map_err(MintError::Other)
        }

        /// Resolve a v4 handle to the object's CURRENT path. The kernel
        /// verifies inode + generation; the path comes from
        /// `/proc/self/fd` on the O_PATH fd, so it reflects renames.
        ///
        /// F52: that readlink is only trusted when it still names the
        /// fd's own inode UNDER the export root. On a cold dcache (a
        /// relocated server's fresh mount) `open_by_handle_at` returns a
        /// DISCONNECTED dentry whose kernel name is "/" — serving that
        /// gave clients EISDIR-as-EIO WRITEs and the container root's
        /// attributes. Untrusted resolutions are re-located by inode
        /// identity; irrecoverable ones answer Stale, never a foreign
        /// path.
        pub fn resolve(&self, data: &[u8]) -> Result<PathBuf, ResolveError> {
            let (fd, dev, ino) = self.open_handle(data)?;
            let path = std::fs::read_link(format!("/proc/self/fd/{}", fd));
            unsafe { libc::close(fd) };
            let path =
                path.map_err(|e| ResolveError::Other(format!("proc readlink: {}", e)))?;
            if super::trusted_resolution(&path, &self.export_root, dev, ino) {
                return Ok(path);
            }
            self.recover_by_identity(dev, ino, &path)
        }

        /// Decode + authenticate + open the handle; returns the O_PATH
        /// fd and its ground-truth inode identity. Caller closes fd.
        fn open_handle(&self, data: &[u8]) -> Result<(i32, u64, u64), ResolveError> {
            let (_ino, htype, kh) = decode_v4(&self.key, self.instance_id, data)
                .map_err(ResolveError::Other)?;
            let mut buf = FileHandleBuf {
                handle_bytes: kh.len() as u32,
                handle_type: htype,
                f_handle: [0u8; MAX_HANDLE_SZ],
            };
            buf.f_handle[..kh.len()].copy_from_slice(&kh);
            let fd = unsafe {
                libc::syscall(
                    libc::SYS_open_by_handle_at,
                    self.mount_fd,
                    &mut buf as *mut FileHandleBuf,
                    libc::O_PATH | libc::O_CLOEXEC,
                )
            } as i32;
            if fd < 0 {
                let err = std::io::Error::last_os_error();
                return match err.raw_os_error() {
                    Some(libc::ESTALE) | Some(libc::ENOENT) => Err(ResolveError::Stale),
                    _ => Err(ResolveError::Other(format!("open_by_handle_at: {}", err))),
                };
            }
            let mut st: libc::stat = unsafe { std::mem::zeroed() };
            if unsafe { libc::fstat(fd, &mut st) } != 0 {
                let e = std::io::Error::last_os_error();
                unsafe { libc::close(fd) };
                return Err(ResolveError::Other(format!("fstat resolved fd: {}", e)));
            }
            Ok((fd, st.st_dev as u64, st.st_ino as u64))
        }

        /// F52 recovery for an untrusted resolution. Logged per event —
        /// the original incident was invisible server-side precisely
        /// because nothing on this path ever spoke up.
        fn recover_by_identity(
            &self,
            dev: u64,
            ino: u64,
            bogus: &Path,
        ) -> Result<PathBuf, ResolveError> {
            match self.identity.locate(dev, ino) {
                Some(p) => {
                    tracing::warn!(
                        "fh resolve: disconnected dentry for ino {} (kernel said {:?}) — \
                         recovered to {:?} by identity walk (F52)",
                        ino,
                        bogus,
                        p
                    );
                    Ok(p)
                }
                None => {
                    tracing::warn!(
                        "fh resolve: ino {} (kernel said {:?}) has no path under \
                         {:?} — answering STALE (F52 belt)",
                        ino,
                        bogus,
                        self.export_root
                    );
                    Err(ResolveError::Stale)
                }
            }
        }

        /// Build the identity index and warm the dcache before the
        /// listener accepts (F52). Returns (entries, complete).
        pub fn prewarm(&self) -> (usize, bool) {
            self.identity.prewarm()
        }

        /// Test-only: resolve while REFUSING to trust the readlink path
        /// — every lookup must go through identity recovery, exactly as
        /// if the dcache were cold. Lets the lima e2e suite exercise the
        /// F52 path without root (a real cold cache needs umount/remount).
        #[cfg(test)]
        pub(crate) fn resolve_as_if_disconnected(
            &self,
            data: &[u8],
        ) -> Result<PathBuf, ResolveError> {
            let (fd, dev, ino) = self.open_handle(data)?;
            unsafe { libc::close(fd) };
            self.recover_by_identity(dev, ino, Path::new("/"))
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use super::*;

    /// Non-Linux stub: kernel handles are a Linux feature; dev/test
    /// platforms run the path-handle scheme. `try_new` fails so the
    /// manager's startup fallback engages.
    pub struct KernelFh;

    impl KernelFh {
        pub fn try_new(_export_root: &Path, _instance_id: u64) -> Result<Self, String> {
            Err("kernel filehandles are Linux-only".to_string())
        }
        pub fn mint(&self, _path: &Path) -> Result<Nfs4FileHandle, MintError> {
            Err(MintError::Other("unsupported platform".to_string()))
        }
        pub fn resolve(&self, _data: &[u8]) -> Result<PathBuf, ResolveError> {
            Err(ResolveError::Other("unsupported platform".to_string()))
        }
        pub fn prewarm(&self) -> (usize, bool) {
            (0, false)
        }
    }
}

pub use imp::KernelFh;

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: [u8; 32] = [7u8; 32];

    #[test]
    fn v4_roundtrip_and_ino() {
        let fh = encode_v4(&KEY, 42, 12345, 1, &[9, 9, 9, 9, 1, 2, 3, 4]).unwrap();
        assert_eq!(fh.data[0], 4);
        assert!(fh.data.len() <= 128);
        assert_eq!(v4_ino(&fh.data), Some(12345));
        let (ino, htype, kh) = decode_v4(&KEY, 42, &fh.data).unwrap();
        assert_eq!((ino, htype), (12345, 1));
        assert_eq!(kh, vec![9, 9, 9, 9, 1, 2, 3, 4]);
    }

    #[test]
    fn v4_rejects_tamper_wrong_key_wrong_instance_truncation() {
        let fh = encode_v4(&KEY, 42, 12345, 1, &[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
        // Flip one khandle bit → HMAC failure.
        let mut evil = fh.data.clone();
        *evil.last_mut().unwrap() ^= 1;
        assert!(decode_v4(&KEY, 42, &evil).unwrap_err().contains("authentication"));
        // Flip the advisory ino → HMAC failure (it's covered by the tag).
        let mut evil = fh.data.clone();
        evil[10] ^= 1;
        assert!(decode_v4(&KEY, 42, &evil).unwrap_err().contains("authentication"));
        // Wrong key.
        assert!(decode_v4(&[8u8; 32], 42, &fh.data).is_err());
        // Wrong instance.
        assert!(decode_v4(&KEY, 43, &fh.data).unwrap_err().contains("stale"));
        // Truncated.
        assert!(decode_v4(&KEY, 42, &fh.data[..20]).is_err());
    }

    // ---- F52 identity-recovery machinery (all-platform: pure std::fs) ----

    use std::os::unix::fs::MetadataExt;

    fn ident_of(p: &Path) -> (u64, u64) {
        let md = std::fs::symlink_metadata(p).unwrap();
        (md.dev(), md.ino())
    }

    /// Fresh temp tree:  root/{a, sub/{b, deep/c}}
    fn temp_tree(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("f52_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub/deep")).unwrap();
        std::fs::write(dir.join("a"), b"a").unwrap();
        std::fs::write(dir.join("sub/b"), b"b").unwrap();
        std::fs::write(dir.join("sub/deep/c"), b"c").unwrap();
        dir
    }

    #[test]
    fn walk_ino_index_maps_tree_and_flags_cap() {
        let dir = temp_tree("walk");
        let dev = ident_of(&dir).0;
        let (map, complete) = walk_ino_index(&dir, dev, 1000);
        assert!(complete);
        // root + sub + deep + 3 files
        assert_eq!(map.len(), 6, "every entry indexed: {:?}", map);
        for rel in ["a", "sub", "sub/b", "sub/deep", "sub/deep/c"] {
            let p = dir.join(rel);
            assert_eq!(map.get(&ident_of(&p)), Some(&p), "{}", rel);
        }
        // Cap exhaustion is reported, not silently truncated.
        let (_capped, complete) = walk_ino_index(&dir, dev, 2);
        assert!(!complete);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn find_by_ino_targeted_hits_and_misses() {
        let dir = temp_tree("find");
        let c = dir.join("sub/deep/c");
        assert_eq!(find_by_ino(&dir, ident_of(&c), 10_000), Some(c.clone()));
        // the export root itself resolves too (root-dir handles)
        assert_eq!(find_by_ino(&dir, ident_of(&dir), 10_000), Some(dir.clone()));
        // absent identity → None; ino 0 exists on no real fs
        assert_eq!(find_by_ino(&dir, (ident_of(&dir).0, 0), 10_000), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The trust gate is THE F52 fix: "/" (a disconnected dentry's
    /// kernel name), foreign paths, since-renamed paths, and deleted
    /// paths must all be refused; only the live in-export path passes.
    #[test]
    fn trusted_resolution_gate() {
        let dir = temp_tree("trust");
        let a = dir.join("a");
        let (dev, ino) = ident_of(&a);
        assert!(trusted_resolution(&a, &dir, dev, ino));
        // The exact F52 shape: kernel says "/" for a disconnected dentry.
        assert!(!trusted_resolution(Path::new("/"), &dir, dev, ino));
        // A path under the root that names a DIFFERENT inode (renamed-over).
        let b = dir.join("sub/b");
        assert!(!trusted_resolution(&b, &dir, dev, ino));
        // Outside the export root entirely, even if the inode matched.
        let (tdev, tino) = ident_of(&std::env::temp_dir());
        assert!(!trusted_resolution(&std::env::temp_dir(), &dir, tdev, tino));
        // Deleted: lstat fails → untrusted.
        std::fs::remove_file(&a).unwrap();
        assert!(!trusted_resolution(&a, &dir, dev, ino));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// End-to-end recovery shape of the runai incident: locate by
    /// identity, then a RENAME invalidates the index mid-cooldown — the
    /// stale hit must force a re-walk (postgres's write-temp-then-rename
    /// would otherwise STALE), and an unlink must answer None.
    #[test]
    fn identity_locate_recovers_rename_and_stales_unlink() {
        let dir = temp_tree("locate");
        let r = IdentityResolver::new(dir.clone(), ident_of(&dir).0, 1000);
        let a = dir.join("a");
        let (dev, ino) = ident_of(&a);
        assert_eq!(r.locate(dev, ino), Some(a.clone()));
        // Rename: the indexed path is now a lie; locate must re-walk and
        // return the NEW path even though the cooldown has not elapsed.
        let a2 = dir.join("sub/a-renamed");
        std::fs::rename(&a, &a2).unwrap();
        assert_eq!(r.locate(dev, ino), Some(a2.clone()));
        // Unlink: no path exists — None (upstream answers STALE, and the
        // F17b/c open-fd fallbacks get their turn).
        std::fs::remove_file(&a2).unwrap();
        assert_eq!(r.locate(dev, ino), None);
        // Foreign device is refused outright.
        assert_eq!(r.locate(dev.wrapping_add(1), ino), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A miss against a fresh COMPLETE index must NOT re-walk per probe
    /// (unlink storms would otherwise walk the tree once per second per
    /// stale client handle).
    #[test]
    fn identity_miss_cooldown_skips_rewalk() {
        use std::sync::atomic::Ordering;
        let dir = temp_tree("cooldown");
        let r = IdentityResolver::new(dir.clone(), ident_of(&dir).0, 1000);
        r.prewarm();
        let walks_after_prewarm = r.walks.load(Ordering::Relaxed);
        let dev = ident_of(&dir).0;
        assert_eq!(r.locate(dev, 0), None);
        assert_eq!(r.locate(dev, 0), None);
        assert_eq!(r.locate(dev, 0), None);
        assert_eq!(
            r.walks.load(Ordering::Relaxed),
            walks_after_prewarm,
            "misses within the cooldown of a complete index must not re-walk"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn ident_cap_env_parsing() {
        // No other test touches this var (single-var, no parallel racer).
        std::env::remove_var("FLINT_FH_IDENT_MAX");
        assert_eq!(ident_cap_from_env(), IDENT_CAP_DEFAULT);
        std::env::set_var("FLINT_FH_IDENT_MAX", "12345");
        assert_eq!(ident_cap_from_env(), 12345);
        std::env::set_var("FLINT_FH_IDENT_MAX", "not-a-number");
        assert_eq!(ident_cap_from_env(), IDENT_CAP_DEFAULT);
        std::env::set_var("FLINT_FH_IDENT_MAX", "0");
        assert_eq!(ident_cap_from_env(), 0);
        std::env::remove_var("FLINT_FH_IDENT_MAX");
    }

    /// cap=0 (FLINT_FH_IDENT_MAX=0): no index, no prewarm — recovery
    /// still works via targeted early-exit walks.
    #[test]
    fn identity_targeted_mode_cap_zero() {
        let dir = temp_tree("targeted");
        let r = IdentityResolver::new(dir.clone(), ident_of(&dir).0, 0);
        assert_eq!(r.prewarm(), (0, false));
        let c = dir.join("sub/deep/c");
        let (dev, ino) = ident_of(&c);
        assert_eq!(r.locate(dev, ino), Some(c));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn key_created_once_and_stable() {
        let dir = std::env::temp_dir().join(format!("fhkey_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let k1 = load_or_create_key(&dir).unwrap();
        let k2 = load_or_create_key(&dir).unwrap();
        assert_eq!(k1, k2, "key must be stable across reloads");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// End-to-end on the real kernel API — only meaningful on Linux
    /// (runs in the lima suite): mint, resolve, rename-survival,
    /// unlink→ESTALE.
    #[cfg(target_os = "linux")]
    #[test]
    fn kernel_mint_resolve_rename_stale() {
        let dir = std::env::temp_dir().join(format!("fhk_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let k = match KernelFh::try_new(&dir, 7) {
            Ok(k) => k,
            // tmpfs (no export_operations) or missing cap — skip, the
            // lima suite runs this on ext4 with the cap granted.
            Err(e) => {
                eprintln!("skipping kernel handle e2e: {}", e);
                return;
            }
        };
        let f = dir.join("obj");
        std::fs::write(&f, b"x").unwrap();
        let fh = match k.mint(&f) {
            Ok(fh) => fh,
            Err(MintError::NoEnt) => panic!("object exists"),
            Err(MintError::Other(e)) => panic!("{}", e),
        };
        assert_eq!(k.resolve(&fh.data).unwrap(), f);
        // F52: force the identity-recovery path — as if the dcache were
        // cold and readlink had answered "/". The true path must come
        // back from the walk, never the kernel's disconnected name.
        // (A REAL cold cache needs root for umount/remount; the repro in
        // the F52 evidence bundle covers that half.)
        let (n, complete) = k.prewarm();
        assert!(complete && n >= 2, "prewarm indexed {} entries", n);
        assert_eq!(
            k.resolve_as_if_disconnected(&fh.data).unwrap(),
            f,
            "identity recovery must find the true path"
        );
        let g = dir.join("obj2");
        std::fs::rename(&f, &g).unwrap();
        assert_eq!(k.resolve(&fh.data).unwrap(), g, "handle follows rename");
        assert_eq!(
            k.resolve_as_if_disconnected(&fh.data).unwrap(),
            g,
            "identity recovery follows renames too"
        );
        std::fs::remove_file(&g).unwrap();
        assert!(matches!(k.resolve(&fh.data), Err(ResolveError::Stale)));
        assert!(
            matches!(k.resolve_as_if_disconnected(&fh.data), Err(ResolveError::Stale)),
            "unlinked: recovery answers Stale, never a foreign path"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
