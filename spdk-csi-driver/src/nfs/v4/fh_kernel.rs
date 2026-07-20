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
    /// and the instance id stamped into every handle.
    pub struct KernelFh {
        mount_fd: i32,
        key: [u8; 32],
        instance_id: u64,
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
            let this = Self {
                mount_fd: fd,
                key,
                instance_id,
            };
            let probe_obj = export_root.join(".flint-nfs").join("fh.key");
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
        pub fn resolve(&self, data: &[u8]) -> Result<PathBuf, ResolveError> {
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
            let path = std::fs::read_link(format!("/proc/self/fd/{}", fd));
            unsafe { libc::close(fd) };
            path.map_err(|e| ResolveError::Other(format!("proc readlink: {}", e)))
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
        let g = dir.join("obj2");
        std::fs::rename(&f, &g).unwrap();
        assert_eq!(k.resolve(&fh.data).unwrap(), g, "handle follows rename");
        std::fs::remove_file(&g).unwrap();
        assert!(matches!(k.resolve(&fh.data), Err(ResolveError::Stale)));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
