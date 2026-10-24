//! Lever E: the scan walks the tree with several threads. It must find
//! exactly what the one-thread walk found (the same entries under the same
//! exclusions) and fail exactly where it failed, except where the syncer
//! may not read, which it now holds (`Scan::held`), the same set at any
//! thread count. The oracle is the walk as it shipped before E, kept here
//! verbatim.

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
    // A name that is not UTF-8: its relative path is the lossy one.
    use std::os::unix::ffi::OsStrExt;
    std::fs::write(root.join("dir").join(std::ffi::OsStr::from_bytes(b"caf\xe9.txt")), "latin-1").unwrap();
    // A modification time before the epoch reads as (0, 0).
    put(root, "dir/old.txt", "old");
    let old = std::ffi::CString::new(root.join("dir/old.txt").to_str().unwrap()).unwrap();
    let times = [libc::timespec { tv_sec: 0, tv_nsec: libc::UTIME_OMIT }, libc::timespec { tv_sec: -100, tv_nsec: 5 }];
    assert_eq!(unsafe { libc::utimensat(libc::AT_FDCWD, old.as_ptr(), times.as_ptr(), 0) }, 0);
}

#[test]
fn the_parallel_walk_finds_what_the_sequential_walk_found() {
    let dir = tempfile::tempdir().unwrap();
    awkward_tree(dir.path());
    let want = reference(dir.path());
    assert!(want.iter().any(|e| e.0 == format!("dir/{STATE_DIR}/kept.txt")), "fixture: depth exclusion");
    assert!(!want.iter().any(|e| e.0.ends_with(TMP_SUFFIX)), "fixture: temporaries");
    assert!(want.iter().any(|e| e.0 == "dir/caf\u{FFFD}.txt"), "fixture: a name that is not UTF-8");
    assert!(want.iter().any(|e| e.0 == "dir/old.txt" && (e.2, e.3) == (0, 0)), "fixture: before the epoch");
    for threads in [1, 2, 8] {
        let got = scan_with(dir.path(), threads).unwrap();
        assert!(got.held.is_empty(), "{threads} threads: held {:?}", got.held);
        assert_eq!(found(&got.entries), want, "{threads} threads");
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
            assert_eq!(found(&scan_with(dir.path(), threads).unwrap().entries), want, "shape {shape}, {threads} threads");
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
            let got = scan_with(dir.path(), threads).unwrap().entries;
            assert_eq!(got.len(), 33, "{threads} threads");
            assert_eq!(got["a/b"].size, 3, "{threads} threads: kept the other name");
        }
    }
}

/// An unreadable directory deep in a tree that keeps the other workers
/// busy is HELD, whichever worker met it: the walk finds everything else,
/// and the held set is the same at any thread count.
#[test]
fn a_directory_no_worker_can_list_is_held_at_any_thread_count() {
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
    let want: Vec<_> =
        reference(dir.path()).into_iter().filter(|e| !e.0.starts_with("d31/deeper/locked/")).collect();
    assert_eq!(want.len(), 64 * 50, "fixture");
    for (threads, g) in [1, 2, 8].iter().zip(got) {
        let g = g.unwrap_or_else(|e| panic!("{threads} threads: an unlistable directory failed the walk: {e}"));
        assert_eq!(g.held.iter().collect::<Vec<_>>(), ["d31/deeper/locked/"], "{threads} threads");
        assert_eq!(found(&g.entries), want, "{threads} threads");
    }
}

/// A directory the syncer may list but not search (`r--`): its names are
/// read, but none can be lstat'ed, so whether each is a file or a
/// directory is unknown too. Each name is held, without a trailing `/`,
/// and covers whatever is under it.
#[test]
fn a_directory_listable_but_not_searchable_holds_each_name() {
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("SKIPPED: running as root, mode bits cannot induce EACCES");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    put(dir.path(), "ok.txt", "x");
    put(dir.path(), "half/a.txt", "x");
    put(dir.path(), "half/sub/b.txt", "x");
    let half = dir.path().join("half");
    std::fs::set_permissions(&half, std::os::unix::fs::PermissionsExt::from_mode(0o444)).unwrap();
    let got: Vec<_> = [1, 2, 8].iter().map(|t| scan_with(dir.path(), *t)).collect();
    std::fs::set_permissions(&half, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    for (threads, g) in [1, 2, 8].iter().zip(got) {
        let g = g.unwrap_or_else(|e| panic!("{threads} threads: {e}"));
        assert_eq!(g.held.iter().collect::<Vec<_>>(), ["half/a.txt", "half/sub"], "{threads} threads");
        assert_eq!(g.entries.keys().collect::<Vec<_>>(), ["ok.txt"], "{threads} threads");
        assert!(g.holds("half/sub/b.txt"), "{threads} threads");
    }
}

/// Every errno but EACCES still fails the walk: an error the tree's owner
/// did not choose is no standing condition to publish around. A directory
/// whose full path is longer than PATH_MAX is one the walk can be handed
/// deterministically: it is listed by that path, and that fails
/// ENAMETOOLONG. Built a level at a time through descriptors, as no full
/// path could be.
#[test]
fn an_error_other_than_eacces_still_fails_the_walk() {
    let dir = tempfile::tempdir().unwrap();
    put(dir.path(), "ok.txt", "x");
    let name = std::ffi::CString::new("n".repeat(200)).unwrap();
    let root = std::ffi::CString::new(dir.path().to_str().unwrap()).unwrap();
    let mut fd = unsafe { libc::open(root.as_ptr(), libc::O_DIRECTORY | libc::O_RDONLY) };
    assert!(fd >= 0, "fixture: open the root");
    for _ in 0..25 {
        assert_eq!(unsafe { libc::mkdirat(fd, name.as_ptr(), 0o755) }, 0, "fixture: mkdirat");
        let next = unsafe { libc::openat(fd, name.as_ptr(), libc::O_DIRECTORY | libc::O_RDONLY) };
        assert!(next >= 0, "fixture: openat");
        unsafe { libc::close(fd) };
        fd = next;
    }
    unsafe { libc::close(fd) };
    for threads in [1, 2, 8] {
        match scan_with(dir.path(), threads) {
            Err(crate::LeanError::Io(e)) => {
                assert_eq!(e.raw_os_error(), Some(libc::ENAMETOOLONG), "{threads} threads: {e}")
            }
            Err(e) => panic!("{threads} threads: failed, but not on the long path: {e}"),
            Ok(s) => panic!(
                "{threads} threads: a walk that could not list a directory returned {} entries, held {:?}",
                s.entries.len(),
                s.held
            ),
        }
    }
}
