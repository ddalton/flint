//! F67: the durable `file_id ↔ path` binding, stored ON the stub file.
//!
//! A striped file's data lives on the DSes in stripe files named by
//! `file_id`; the binding from path to that id historically existed only
//! in the state backend. Lose the backend while the export tree survives
//! and the MDS mints a FRESH id over existing data — every byte then
//! reads as silent zeros through the DS hole path (proven end-to-end,
//! 2026-08-03: post-restart md5 == md5 of pure zeros, 0 wire bytes).
//!
//! This module makes the binding a sibling of the stub itself: a user
//! xattr (`user.flint.placement`) written BEFORE the backend record, so
//! the binding lives in the same failure domain as the namespace entry
//! it serves. See `docs/plans/f67-durable-placement-binding.md`.

use std::path::{Path, PathBuf};

use super::layout::FilePlacement;

/// Xattr name carrying the v1 binding. The `user.` prefix is meaningful
/// on Linux (unprivileged namespace); on macOS (the lima rig) names are
/// free-form and the same string is used verbatim.
pub const BINDING_XATTR: &str = "user.flint.placement";

/// Size + allocated-blocks view of a stub, for the orphan guard. A
/// striped file's stub is fully sparse (`blocks == 0`); an MDS-native
/// file with real data has blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StubMeta {
    pub len: u64,
    pub blocks: u64,
}

/// The layout manager's window onto stub files. `layout.rs` stays
/// filesystem-free; the server injects the real implementation rooted
/// at its export path.
pub trait StubBinding: Send + Sync {
    /// `None` = no stub exists for this key.
    fn stub_meta(&self, file_key: &str) -> Option<StubMeta>;
    /// Read the binding xattr back as a placement. `None` covers
    /// "no stub", "no xattr", and "unparseable" (the last logs).
    fn read(&self, file_key: &str) -> Option<FilePlacement>;
    /// Write the binding xattr. MUST succeed before a freshly minted
    /// placement may be used — the caller refuses the grant otherwise.
    fn write(&self, file_key: &str, placement: &FilePlacement) -> std::io::Result<()>;
}

/// Serialize as `v1:{file_id:016x}:{stripe_size}:{dev1,dev2,...}`.
/// Device ORDER is load-bearing (stripe unit → device mapping), so the
/// format preserves it verbatim.
pub fn serialize(p: &FilePlacement) -> String {
    format!(
        "v1:{:016x}:{}:{}",
        p.file_id,
        p.stripe_size,
        p.device_ids.join(",")
    )
}

/// Strict parse of the v1 format. Unknown versions return `None` — the
/// caller treats that as "no binding", and the orphan guard still
/// protects the data (a nonzero stub without a binding is refused, not
/// re-minted).
pub fn parse(raw: &str) -> Option<FilePlacement> {
    let mut it = raw.splitn(4, ':');
    if it.next()? != "v1" {
        return None;
    }
    let file_id = u64::from_str_radix(it.next()?, 16).ok()?;
    let stripe_size: u64 = it.next()?.parse().ok()?;
    let devs = it.next()?;
    if file_id == 0 || stripe_size == 0 || devs.is_empty() {
        return None;
    }
    Some(FilePlacement {
        stripe_size,
        device_ids: devs.split(',').map(str::to_string).collect(),
        file_id,
    })
}

/// The real binding: xattrs on stub files under `export_root`.
pub struct XattrStubBinding {
    export_root: PathBuf,
}

impl XattrStubBinding {
    pub fn new(export_root: PathBuf) -> Self {
        Self { export_root }
    }

    /// `file_key` is the export-relative path (leading '/' tolerated).
    fn stub_path(&self, file_key: &str) -> PathBuf {
        self.export_root.join(file_key.trim_start_matches('/'))
    }

    /// Can this filesystem hold user xattrs? Probed once at startup on
    /// the export ROOT (a directory — xattr support is per-fs, not
    /// per-file). A `memory` state backend on an xattr-less fs must
    /// refuse to boot: that combination is "restart = silent zeros".
    pub fn probe(export_root: &Path) -> bool {
        let val = b"probe";
        set_xattr(export_root, "user.flint.probe", val).is_ok()
            && get_xattr(export_root, "user.flint.probe").as_deref() == Some(&val[..])
    }
}

impl StubBinding for XattrStubBinding {
    fn stub_meta(&self, file_key: &str) -> Option<StubMeta> {
        use std::os::unix::fs::MetadataExt;
        let md = std::fs::symlink_metadata(self.stub_path(file_key)).ok()?;
        if !md.is_file() {
            return None;
        }
        Some(StubMeta { len: md.len(), blocks: md.blocks() })
    }

    fn read(&self, file_key: &str) -> Option<FilePlacement> {
        let raw = get_xattr(&self.stub_path(file_key), BINDING_XATTR)?;
        let s = String::from_utf8(raw).ok()?;
        let parsed = parse(&s);
        if parsed.is_none() {
            tracing::error!(
                "F67: stub '{}' carries an unparseable placement binding ({:?}) — \
                 treating as absent; the orphan guard will refuse I/O rather than re-mint",
                file_key, s
            );
        }
        parsed
    }

    fn write(&self, file_key: &str, placement: &FilePlacement) -> std::io::Result<()> {
        set_xattr(
            &self.stub_path(file_key),
            BINDING_XATTR,
            serialize(placement).as_bytes(),
        )
    }
}

/// Test/default binding: a world with no stubs. Every lookup misses,
/// every meta is absent (so the guard never trips), writes succeed.
/// Production code must NOT use this — the server wires
/// [`XattrStubBinding`]; this exists so unit tests of unrelated layout
/// behavior keep their pre-F67 semantics.
pub struct NoStubs;

impl StubBinding for NoStubs {
    fn stub_meta(&self, _file_key: &str) -> Option<StubMeta> {
        None
    }
    fn read(&self, _file_key: &str) -> Option<FilePlacement> {
        None
    }
    fn write(&self, _file_key: &str, _placement: &FilePlacement) -> std::io::Result<()> {
        Ok(())
    }
}

// ── raw xattr syscalls (Linux glibc/musl + macOS for the lima rig) ──────

fn cpath(p: &Path) -> std::io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(p.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in path"))
}

#[cfg(target_os = "linux")]
fn set_xattr(path: &Path, name: &str, value: &[u8]) -> std::io::Result<()> {
    let p = cpath(path)?;
    let n = std::ffi::CString::new(name).unwrap();
    let rc = unsafe {
        libc::setxattr(
            p.as_ptr(),
            n.as_ptr(),
            value.as_ptr() as *const libc::c_void,
            value.len(),
            0,
        )
    };
    if rc == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

#[cfg(target_os = "linux")]
fn get_xattr(path: &Path, name: &str) -> Option<Vec<u8>> {
    let p = cpath(path).ok()?;
    let n = std::ffi::CString::new(name).ok()?;
    let mut buf = vec![0u8; 512];
    let rc = unsafe {
        libc::getxattr(
            p.as_ptr(),
            n.as_ptr(),
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len(),
        )
    };
    if rc < 0 {
        return None;
    }
    buf.truncate(rc as usize);
    Some(buf)
}

#[cfg(target_os = "macos")]
fn set_xattr(path: &Path, name: &str, value: &[u8]) -> std::io::Result<()> {
    let p = cpath(path)?;
    let n = std::ffi::CString::new(name).unwrap();
    let rc = unsafe {
        libc::setxattr(
            p.as_ptr(),
            n.as_ptr(),
            value.as_ptr() as *const libc::c_void,
            value.len(),
            0, // position (resource forks only)
            0, // options
        )
    };
    if rc == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

#[cfg(target_os = "macos")]
fn get_xattr(path: &Path, name: &str) -> Option<Vec<u8>> {
    let p = cpath(path).ok()?;
    let n = std::ffi::CString::new(name).ok()?;
    let mut buf = vec![0u8; 512];
    let rc = unsafe {
        libc::getxattr(
            p.as_ptr(),
            n.as_ptr(),
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len(),
            0, // position
            0, // options
        )
    };
    if rc < 0 {
        return None;
    }
    buf.truncate(rc as usize);
    Some(buf)
}

#[cfg(test)]
pub mod test_support {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// In-memory binding for layout tests: script stub metas, capture
    /// writes, optionally fail them.
    #[derive(Default)]
    pub struct MemoryStubBinding {
        pub metas: Mutex<HashMap<String, StubMeta>>,
        pub bindings: Mutex<HashMap<String, FilePlacement>>,
        pub fail_writes: std::sync::atomic::AtomicBool,
    }

    impl StubBinding for MemoryStubBinding {
        fn stub_meta(&self, file_key: &str) -> Option<StubMeta> {
            self.metas.lock().unwrap().get(file_key).copied()
        }
        fn read(&self, file_key: &str) -> Option<FilePlacement> {
            self.bindings.lock().unwrap().get(file_key).cloned()
        }
        fn write(&self, file_key: &str, placement: &FilePlacement) -> std::io::Result<()> {
            if self.fail_writes.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(std::io::Error::new(std::io::ErrorKind::Other, "scripted failure"));
            }
            self.bindings
                .lock()
                .unwrap()
                .insert(file_key.to_string(), placement.clone());
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn placement() -> FilePlacement {
        FilePlacement {
            stripe_size: 8 * 1024 * 1024,
            device_ids: vec!["ds-b".into(), "ds-a".into(), "ds-c".into()],
            file_id: 0x00b97e4be38c246d,
        }
    }

    #[test]
    fn serialize_parse_round_trip_preserves_device_order() {
        let p = placement();
        let restored = parse(&serialize(&p)).expect("round trip");
        assert_eq!(restored, p, "device ORDER is load-bearing and must survive");
    }

    #[test]
    fn parse_rejects_wrong_version_zero_id_and_garbage() {
        assert!(parse("v2:0000000000000001:8388608:ds-a").is_none(), "unknown version");
        assert!(parse("v1:0000000000000000:8388608:ds-a").is_none(), "zero file_id");
        assert!(parse("v1:0000000000000001:0:ds-a").is_none(), "zero stripe");
        assert!(parse("v1:0000000000000001:8388608:").is_none(), "empty devices");
        assert!(parse("").is_none());
        assert!(parse("not-a-binding").is_none());
    }

    #[test]
    fn xattr_binding_round_trips_on_a_real_filesystem() {
        let dir = tempfile::tempdir().unwrap();
        if !XattrStubBinding::probe(dir.path()) {
            eprintln!("skipping: no user-xattr support on this test fs");
            return;
        }
        let stub = dir.path().join("vol/f1");
        std::fs::create_dir_all(stub.parent().unwrap()).unwrap();
        std::fs::write(&stub, b"").unwrap();

        let b = XattrStubBinding::new(dir.path().to_path_buf());
        assert!(b.read("/vol/f1").is_none(), "no binding yet");
        b.write("/vol/f1", &placement()).unwrap();
        assert_eq!(b.read("/vol/f1"), Some(placement()));

        let meta = b.stub_meta("/vol/f1").unwrap();
        assert_eq!(meta.len, 0);
    }

    #[test]
    fn stub_meta_reports_sparse_vs_dense() {
        let dir = tempfile::tempdir().unwrap();
        let b = XattrStubBinding::new(dir.path().to_path_buf());

        assert!(b.stub_meta("/absent").is_none());

        // sparse: set_len only — the striped-stub shape
        let sparse = dir.path().join("sparse");
        let f = std::fs::File::create(&sparse).unwrap();
        f.set_len(4 * 1024 * 1024).unwrap();
        drop(f);
        let m = b.stub_meta("/sparse").unwrap();
        assert_eq!(m.len, 4 * 1024 * 1024);
        assert_eq!(m.blocks, 0, "a striped stub allocates nothing");

        // dense: real bytes — the MDS-native shape
        let dense = dir.path().join("dense");
        std::fs::write(&dense, vec![7u8; 128 * 1024]).unwrap();
        let m = b.stub_meta("/dense").unwrap();
        assert_eq!(m.len, 128 * 1024);
        assert!(m.blocks > 0, "native data has blocks");
    }
}
