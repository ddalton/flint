//! Lever E: the scan walks the tree with several threads. It must find
//! exactly what the one-thread walk found (the same entries under the same
//! exclusions) and fail exactly where it failed. The oracle is the walk as
//! it shipped before E, kept here verbatim.

use super::scan::{scan_with, ScanEntry, TMP_SUFFIX};
use super::{CONTROL_DIR, STATE_DIR};
use std::collections::BTreeMap;
use std::path::Path;

/// The walk before lever E, verbatim but for its name: sequential, every
/// entry `lstat`ed by its full path.
fn reference_walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, ScanEntry>) -> crate::LeanResult<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        if dir == root {
            let n = name.to_string_lossy();
            if n == STATE_DIR || n == CONTROL_DIR {
                continue;
            }
        }
        if name.to_string_lossy().ends_with(TMP_SUFFIX) {
            continue;
        }
        let meta = std::fs::symlink_metadata(&path)?;
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            reference_walk(root, &path, out)?;
        } else if meta.is_file() {
            let rel = path.strip_prefix(root).expect("walk stays under root").to_string_lossy().replace('\\', "/");
            use std::os::unix::fs::MetadataExt;
            let mode = meta.mode();
            let (mtime_unix, mtime_nanos) = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| (d.as_secs() as i64, d.subsec_nanos()))
                .unwrap_or((0, 0));
            out.insert(rel, ScanEntry { size: meta.len(), mtime_unix, mtime_nanos, mode, seen_nanos: 0 });
        }
    }
    Ok(())
}

/// What a walk found, without when it looked.
fn found(m: &BTreeMap<String, ScanEntry>) -> Vec<(String, u64, i64, u32, u32)> {
    m.iter().map(|(p, e)| (p.clone(), e.size, e.mtime_unix, e.mtime_nanos, e.mode)).collect()
}

fn reference(root: &Path) -> Vec<(String, u64, i64, u32, u32)> {
    let mut out = BTreeMap::new();
    reference_walk(root, root, &mut out).unwrap();
    found(&out)
}

fn put(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

/// Every exclusion and every kind of entry the walk distinguishes.
fn awkward_tree(root: &Path) {
    put(root, "a.txt", "a");
    put(root, "dir/b.txt", "bb");
    put(root, "dir/deep/er/c.txt", "ccc");
    put(root, "dir/deep/er/still/d.bin", "dddd");
    // Excluded at the root only: at depth they are ordinary data.
    put(root, &format!("{STATE_DIR}/baseline.json"), "{}");
    put(root, &format!("{CONTROL_DIR}/publish"), "");
    put(root, &format!("dir/{STATE_DIR}/kept.txt"), "kept");
    put(root, &format!("dir/{CONTROL_DIR}/kept.txt"), "kept");
    // The syncer's temporaries, at any depth.
    put(root, &format!("x{TMP_SUFFIX}"), "tmp");
    put(root, &format!("dir/deep/y{TMP_SUFFIX}"), "tmp");
    // Symlinks, to a file and to a directory: skipped, never followed.
    std::os::unix::fs::symlink(root.join("a.txt"), root.join("link-to-file")).unwrap();
    std::os::unix::fs::symlink(root.join("dir"), root.join("dir/deep/link-to-dir")).unwrap();
    // Not a regular file: skipped.
    let fifo = std::ffi::CString::new(root.join("dir/a-fifo").to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);
    // Empty directories, and a name with a backslash (the relative path's
    // `\` becomes `/`, as the reference does).
    std::fs::create_dir_all(root.join("empty/emptier")).unwrap();
    put(root, "dir/back\\slash.txt", "b");
    // Mode bits are part of what is found.
    put(root, "dir/run.sh", "#!/bin/sh");
    std::fs::set_permissions(root.join("dir/run.sh"), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
}

#[test]
fn the_parallel_walk_finds_what_the_sequential_walk_found() {
    let dir = tempfile::tempdir().unwrap();
    awkward_tree(dir.path());
    let want = reference(dir.path());
    assert!(want.iter().any(|e| e.0 == format!("dir/{STATE_DIR}/kept.txt")), "fixture: depth exclusion");
    assert!(!want.iter().any(|e| e.0.ends_with(TMP_SUFFIX)), "fixture: temporaries");
    for threads in [1, 2, 8] {
        let got = found(&scan_with(dir.path(), threads).unwrap());
        assert_eq!(got, want, "{threads} threads");
    }
}

/// Seeded random trees: wide, deep, and one huge directory.
#[test]
fn random_trees_walk_the_same_at_any_thread_count() {
    let mut rng: u64 = 0x2545_F491_4F6C_DD1D;
    let mut next = |n: u64| {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng % n
    };
    for shape in 0..6u64 {
        let dir = tempfile::tempdir().unwrap();
        let dirs = [1, 3, 40, 200, 7, 120][shape as usize];
        let files = [3000, 500, 4000, 2000, 50, 3000][shape as usize];
        let mut paths: Vec<String> = vec![String::new()];
        for d in 0..dirs {
            let parent = paths[next(paths.len() as u64) as usize].clone();
            paths.push(format!("{parent}d{d}/"));
        }
        for f in 0..files {
            let parent = &paths[next(paths.len() as u64) as usize];
            put(dir.path(), &format!("{parent}f{f}.txt"), &"x".repeat(next(64) as usize));
        }
        let want = reference(dir.path());
        assert_eq!(want.len(), files as usize, "fixture: shape {shape}");
        for threads in [1, 3, 8] {
            assert_eq!(found(&scan_with(dir.path(), threads).unwrap()), want, "shape {shape}, {threads} threads");
        }
    }
}

/// `a\b` at the root and `a/b` are one relative path. The one-thread walk
/// kept whichever `read_dir` listed later; workers would keep whichever
/// they happened to read later, a different file from one scan to the
/// next. The rule is the tree's: the larger stat.
#[test]
fn two_names_for_one_path_keep_the_same_one_every_walk() {
    let dir = tempfile::tempdir().unwrap();
    put(dir.path(), "a\\b", "x");
    put(dir.path(), "a/b", "xyz");
    for d in 0..32 {
        put(dir.path(), &format!("a/d{d}/f.txt"), "keeps the workers busy");
    }
    for threads in [1, 2, 8] {
        for _ in 0..20 {
            let got = scan_with(dir.path(), threads).unwrap();
            assert_eq!(got.len(), 33, "{threads} threads");
            assert_eq!(got["a/b"].size, 3, "{threads} threads: kept the other name");
        }
    }
}

/// An unreadable directory deep in a tree that keeps the other workers
/// busy: the error is the walk's, whichever worker met it.
#[test]
fn a_directory_no_worker_can_list_fails_the_whole_walk() {
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("SKIPPED: running as root, mode bits cannot induce EACCES");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    for d in 0..64 {
        for f in 0..50 {
            put(dir.path(), &format!("d{d}/f{f}.txt"), "x");
        }
    }
    put(dir.path(), "d31/deeper/locked/f.txt", "still here");
    let locked = dir.path().join("d31/deeper/locked");
    std::fs::set_permissions(&locked, std::os::unix::fs::PermissionsExt::from_mode(0o000)).unwrap();
    let got: Vec<_> = [1, 2, 8].iter().map(|t| scan_with(dir.path(), *t)).collect();
    std::fs::set_permissions(&locked, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    for (threads, g) in [1, 2, 8].iter().zip(got) {
        match g {
            Err(crate::LeanError::Io(e)) => assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied, "{threads} threads: {e}"),
            Err(e) => panic!("{threads} threads: failed, but not on the directory: {e}"),
            Ok(s) => panic!("{threads} threads: an unlistable directory scanned as {} entries", s.len()),
        }
    }
}
