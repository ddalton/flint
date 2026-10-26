//! The hostile-tree fuzzer. The lean syncer's recent bugs live where it
//! meets the real filesystem, which the formal model abstracts away:
//! names, modes, mtimes, permissions, and a tree that changes under the
//! walk. L-138, L-139, L-140, L-141 and L-142 each let ONE file stall or
//! silently mis-publish a workspace, and each was found by hand, one
//! ledger row at a time. This generates the class instead.
//!
//! A seeded case grows a tree out of everything the agent contract
//! (`AGENTS.md`) allows or says how it handles: odd and keyless names,
//! every mode, symlinks, FIFOs, sockets, hard links, weird mtimes, long
//! paths. The UI saves and deletes through the gateway's commit. Inside
//! a barrier, between the walk and the open, one file is mutated. After
//! every barrier the document and the tree are checked against a
//! snapshot of the disk that sees through every permission:
//!
//! - LIVENESS: the barrier returns Ok.
//! - PUBLISH: every file the walk sees and may read is cited with its
//!   bytes and permission bits; a UI write is cited as the UI wrote it.
//! - HELD: a cited path the walk may not see or read keeps its citation.
//! - NO EARLY DELETE: a citation goes only if the UI removed it or the
//!   path was absent from two consecutive scans.
//! - NO STALE CITATION: a new citation is of a file the walk saw; an old
//!   one stays only while the path is present or is a first absence.
//! - HONEST HOLDS: `report.held` is exactly what the walk may not read
//!   or name, and every unreadable file.
//! - TREE INTEGRITY: the barrier changed nothing on disk but what the UI
//!   wrote or removed.
//!
//! Each case ends quiet (a third barrier with nothing to do), then
//! unlocks everything and requires the document to equal the tree, then
//! checks out a fresh tree that must equal the document and publish
//! nothing, and checks that no store key is longer than S3's 1024 bytes.
//!
//! `FLINT_FUZZ_SEED` (a number, or `random`), `FLINT_FUZZ_CASES`,
//! `FLINT_FUZZ_TICKS` and `FLINT_FUZZ_CASE` (one case, to replay) steer
//! it; a failure prints the seed, the case and every operation. A
//! finding the generator knows (`KNOWN`) is not generated unless
//! `FLINT_FUZZ_KNOWN` names it (or is `all`). Run it as a non-root uid:
//! as root no mode bit refuses a read, and the permission half is off
//! (it prints SKIPPED).

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{CString, OsStr};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use flint_store::memory::MemoryStore;
use flint_store::{crc64_nvme, crc64_to_b64, ObjectStore};

use super::barrier::{BarrierReport, CONSUME_WINDOW_HOOK};
use super::manifest::{self, LeanEntry};
use super::scan::TMP_SUFFIX;
use super::tests::{hitl_remove, hitl_write, syncer};
use super::{LeanConfig, Syncer, CONTROL_DIR, STATE_DIR};

/// A finding the fuzzer has met and the ledger records. Its trigger is
/// not generated unless `FLINT_FUZZ_KNOWN` names it, so the battery stays
/// green while the finding is open, and says so every run.
struct Known {
    name: &'static str,
    what: &'static str,
}

const KNOWN: &[Known] = &[
    Known {
        name: "enoent-at-open",
        what: "L-144: a name removed while the barrier runs fails the whole barrier",
    },
    Known {
        name: "suid-lost-on-write",
        what: "L-146: a checkout or consume sets the mode before writing, and the write clears setuid/setgid",
    },
    Known {
        name: "long-keys",
        what: "L-145: a path over ~968 bytes makes a handle key over S3's 1024-byte limit",
    },
    Known {
        name: "lossy-hold-collision",
        what: "L-141, accepted: a keyless name held under its lossy form also holds a real name spelled with U+FFFD",
    },
    Known {
        name: "agent-set-mtime",
        what: "AGENTS.md: a same-size rewrite whose mtime the agent set to the published one is invisible (ctime would see it)",
    },
    Known {
        name: "file-hold-covers-subtree",
        what: "L-147: an unreadable file where a cited directory was keeps the citations under it, forever a first absence",
    },
];

/// The longest relative path the generator makes while `long-keys` is
/// not generated: a handle key adds the prefix, an `@` and a flush id
/// (`ui-<secs>-<uuid>` the longest), and must stay within 1024 bytes.
const SAFE_PATH: usize = 940;

fn known_enabled(name: &str) -> bool {
    let v = std::env::var("FLINT_FUZZ_KNOWN").unwrap_or_default();
    v == "all" || v.split(',').any(|n| n == name)
}

// ── the generator's dice ─────────────────────────────────────────────

/// splitmix64: small, seedable, and the same on every platform.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
    fn pct(&mut self, p: u64) -> bool {
        self.next() % 100 < p
    }
    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len())]
    }
}

/// Names the gateway's `path_ok` takes, so the UI may write them too.
const UI_NAMES: &[&str] = &[
    "a", "b.txt", "README.md", "src", "data.bin", "a\\b", "\\", "with space", " lead", "trail ",
    "new\nline", "tab\there", "%41", "q?", "h#", "star*", "at@", "f@ui-1",
    "n@ui-1700000000-00000000-0000-0000-0000-000000000000", "\u{65e5}\u{672c}\u{8a9e}", "caf\u{e9}",
    "cafe\u{301}", "\u{202e}txt.exe", "zero\u{200b}width", ".hidden", "..dots", "...", "caf\u{fffd}",
    "-dash", "UPPER", "upper", "emoji\u{1f600}", ".flint", "~", "@",
];

/// Names that are not UTF-8: held, never published (L-141).
const KEYLESS: &[&[u8]] = &[b"caf\xe9", b"\xff", b"trunc\xc3", b"sur\xed\xa0\x80", b"\xfe\xff.txt", b"caf\xe9.d"];

const FILE_MODES: &[u32] = &[
    0o644, 0o644, 0o644, 0o644, 0o600, 0o755, 0o755, 0o444, 0o000, 0o200, 0o4755, 0o2755, 0o640, 0o1644, 0o2644,
];
const DIR_MODES: &[u32] = &[0o755, 0o755, 0o755, 0o700, 0o000, 0o300, 0o500, 0o600, 0o100];

/// The gateway's `path_ok` (`flint-lean-gateway`, `workspace.rs`), which
/// this crate cannot depend on: what the UI may name.
fn path_ok(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.split('/').any(|seg| {
            seg.is_empty() || seg == "." || seg == ".." || seg == STATE_DIR || seg.ends_with(TMP_SUFFIX)
        })
        && !path.starts_with(".flint/")
        && path != ".flint"
}

// ── the disk, seen through every permission ──────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
enum Node {
    /// `ino` tells a hard link's names apart from copies.
    File { body: Vec<u8>, perm: u32, ino: u64 },
    Dir { perm: u32 },
    Link(Vec<u8>),
    /// A FIFO, a socket: its file-type bits.
    Special(u32),
}

/// Every node under the root but the syncer's own two directories, by
/// its relative path's bytes.
type Snap = BTreeMap<Vec<u8>, Node>;

type Doc = BTreeMap<String, LeanEntry>;

fn abs(root: &Path, rel: &[u8]) -> PathBuf {
    if rel.is_empty() {
        root.to_path_buf()
    } else {
        root.join(OsStr::from_bytes(rel))
    }
}

fn join(dir: &[u8], name: &[u8]) -> Vec<u8> {
    if dir.is_empty() {
        name.to_vec()
    } else {
        [dir, b"/", name].concat()
    }
}

fn parent(p: &[u8]) -> &[u8] {
    p.iter().rposition(|b| *b == b'/').map(|i| &p[..i]).unwrap_or(b"")
}

/// `p` is strictly below `anc` (the root, empty, is above everything).
fn under(p: &[u8], anc: &[u8]) -> bool {
    (anc.is_empty() && !p.is_empty()) || (p.len() > anc.len() && p.starts_with(anc) && p[anc.len()] == b'/')
}

/// One of the two is the other or under it.
fn related(a: &[u8], b: &[u8]) -> bool {
    a == b || under(a, b) || under(b, a)
}

fn ancestors(p: &[u8]) -> impl Iterator<Item = &[u8]> {
    p.iter().enumerate().filter(|(_, b)| **b == b'/').map(move |(i, _)| &p[..i])
}

fn show(p: &[u8]) -> String {
    match std::str::from_utf8(p) {
        Ok(s) => format!("{s:?}"),
        Err(_) => format!("b\"{}\"", p.escape_ascii()),
    }
}

fn chmod(p: &Path, perm: u32) -> std::io::Result<()> {
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(perm))
}

fn crc_b64(body: &[u8]) -> String {
    crc64_to_b64(crc64_nvme(body))
}

/// The tree as it is. A directory or file the owner may not read is
/// opened up for the read and put back: a mode change moves only ctime,
/// which nothing in the syncer reads.
fn snapshot(root: &Path) -> Snap {
    let mut out = Snap::new();
    snap_dir(root, b"", &mut out);
    out
}

fn snap_dir(root: &Path, rel: &[u8], out: &mut Snap) {
    let dir = abs(root, rel);
    let perm = std::fs::symlink_metadata(&dir).unwrap().permissions().mode() & 0o7777;
    let opened = perm | 0o700;
    if opened != perm {
        chmod(&dir, opened).unwrap();
    }
    let mut names: Vec<Vec<u8>> =
        std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().into_vec()).collect();
    names.sort();
    for name in names {
        if rel.is_empty() && (name == STATE_DIR.as_bytes() || name == CONTROL_DIR.as_bytes()) {
            continue;
        }
        let r = join(rel, &name);
        let p = abs(root, &r);
        let m = std::fs::symlink_metadata(&p).unwrap();
        let ft = m.file_type();
        let perm = m.permissions().mode() & 0o7777;
        if ft.is_dir() {
            out.insert(r.clone(), Node::Dir { perm });
            snap_dir(root, &r, out);
        } else if ft.is_file() {
            let body = if perm & 0o400 == 0 {
                chmod(&p, perm | 0o400).unwrap();
                let b = std::fs::read(&p).unwrap();
                chmod(&p, perm).unwrap();
                b
            } else {
                std::fs::read(&p).unwrap()
            };
            out.insert(r, Node::File { body, perm, ino: m.ino() });
        } else if ft.is_symlink() {
            out.insert(r, Node::Link(std::fs::read_link(&p).unwrap().into_os_string().into_vec()));
        } else {
            out.insert(r, Node::Special(m.mode() & 0o170000));
        }
    }
    if opened != perm {
        chmod(&dir, perm).unwrap();
    }
}

/// Every name of the file at `p` (`p` among them): a hard link's names
/// change together.
fn links(s: &Snap, p: &[u8]) -> Vec<Vec<u8>> {
    let Some(Node::File { ino, .. }) = s.get(p) else { return vec![p.to_vec()] };
    s.iter()
        .filter(|(_, n)| matches!(n, Node::File { ino: i, .. } if i == ino))
        .map(|(q, _)| q.clone())
        .collect()
}

fn perm_of(s: &Snap, p: &[u8]) -> u32 {
    if p.is_empty() {
        return 0o755;
    }
    match s.get(p) {
        Some(Node::Dir { perm }) | Some(Node::File { perm, .. }) => *perm,
        _ => 0,
    }
}

/// The owner may resolve `p`: every directory above it is searchable.
fn searchable(s: &Snap, p: &[u8]) -> bool {
    ancestors(p).all(|a| perm_of(s, a) & 0o100 != 0)
}

/// The owner may make and remove names in `d` without changing a mode:
/// `d` and every directory above it are rwx.
fn open_dir(s: &Snap, d: &[u8]) -> bool {
    (d.is_empty() || matches!(s.get(d), Some(Node::Dir { perm }) if perm & 0o700 == 0o700))
        && ancestors(d).all(|a| perm_of(s, a) & 0o700 == 0o700)
}

// ── what the walk sees (`scan::read_dir_entries`, worked by hand) ────

#[derive(Debug, Clone, PartialEq, Eq)]
enum Vis {
    /// Walked: a file it stats and keys, a directory it lists.
    Seen,
    /// Held: `lossy` is how the walk names it, `at` the path it stands
    /// for (it and everything under it).
    Held { lossy: String, at: Vec<u8> },
    /// A name ending in the temp suffix, or under one: never walked.
    Skipped,
}

fn vis(s: &Snap, p: &[u8]) -> Vis {
    let comps: Vec<&[u8]> = p.split(|b| *b == b'/').collect();
    let last = comps.len() - 1;
    let mut prefix = String::new();
    let mut at: Vec<u8> = vec![];
    // The root is never locked.
    let mut parent_x = true;
    for (i, c) in comps.iter().enumerate() {
        at = join(&at, c);
        let lossy = String::from_utf8_lossy(c);
        let keyless = matches!(lossy, Cow::Owned(_));
        if lossy.ends_with(TMP_SUFFIX) {
            return Vis::Skipped;
        }
        // The fstatat of a name needs search on its directory.
        if !parent_x {
            return Vis::Held { lossy: format!("{prefix}{lossy}"), at };
        }
        match s.get(&at) {
            Some(Node::Dir { perm }) => {
                // A keyless directory is held whole; a directory is opened
                // to be listed, which needs read.
                if keyless || perm & 0o400 == 0 {
                    return Vis::Held { lossy: format!("{prefix}{lossy}/"), at };
                }
                parent_x = perm & 0o100 != 0;
                prefix.push_str(&lossy);
                prefix.push('/');
            }
            Some(Node::File { .. }) if keyless => return Vis::Held { lossy: format!("{prefix}{lossy}"), at },
            Some(_) => {
                assert_eq!(i, last, "fixture: {} is under a non-directory", show(p));
                return Vis::Seen;
            }
            None => panic!("fixture: {} is not in the snapshot", show(&at)),
        }
    }
    Vis::Seen
}

#[derive(Clone, Debug)]
struct Seen {
    crc: String,
    size: u64,
    perm: u32,
}

/// One barrier's scan, as the snapshot predicts it.
#[derive(Clone, Default)]
struct View {
    /// Every keyed regular file the walk stats.
    seen: BTreeMap<String, Seen>,
    /// `Scan::held`, as the walk names them...
    held: BTreeSet<String>,
    /// ...and the paths they stand for.
    held_at: BTreeSet<Vec<u8>>,
    /// Seen files the owner may not read.
    unreadable: BTreeSet<String>,
}

impl View {
    /// `k` is at or under something held: unseen, not absent. By the
    /// path's BYTES, not by the lossy name the walk reports.
    fn covered(&self, k: &str) -> bool {
        self.held_at.iter().any(|h| k.as_bytes() == &h[..] || under(k.as_bytes(), h))
    }
    fn present(&self, k: &str) -> bool {
        self.seen.contains_key(k) || self.covered(k)
    }
}

fn view(s: &Snap) -> View {
    let mut v = View::default();
    for (p, n) in s {
        match vis(s, p) {
            Vis::Held { lossy, at } => {
                v.held.insert(lossy);
                v.held_at.insert(at);
            }
            Vis::Seen => {
                if let Node::File { body, perm, .. } = n {
                    let k = String::from_utf8(p.clone()).expect("a seen path is UTF-8");
                    if perm & 0o400 == 0 {
                        v.unreadable.insert(k.clone());
                    }
                    v.seen.insert(k, Seen { crc: crc_b64(body), size: body.len() as u64, perm: *perm });
                }
            }
            Vis::Skipped => {}
        }
    }
    v
}

// ── the case ─────────────────────────────────────────────────────────

/// What the UI did this tick, through the gateway's one-CAS commit.
#[derive(Clone, Debug)]
enum Ui {
    Write { path: String, body: String, perm: u32 },
    Remove { path: String },
}

impl Ui {
    fn path(&self) -> &str {
        match self {
            Ui::Write { path, .. } | Ui::Remove { path } => path,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InjKind {
    Delete,
    Chmod000,
    SwapDir,
    SwapLink,
    SwapFifo,
    Grow,
    LockParent,
}

/// A mutation inside the barrier, between the walk and the open.
#[derive(Clone, Debug)]
struct Inj {
    kind: InjKind,
    x: Vec<u8>,
    /// What it touched: the file, or for `LockParent` its directory...
    zone: Vec<u8>,
    /// ...and the file's other names.
    also: Vec<Vec<u8>>,
}

struct Case {
    seed: u64,
    idx: usize,
    rng: Rng,
    root: PathBuf,
    perms: bool,
    /// `SAFE_PATH`, or no limit while `long-keys` is generated.
    max_path: usize,
    log: Vec<String>,
}

impl Case {
    fn note(&mut self, line: String) {
        self.log.push(line);
    }

    fn fail(&self, tick: &str, why: String) -> String {
        format!(
            "FUZZ FAILURE seed={} case={} at {tick}: {why}\n  replay: FLINT_FUZZ_SEED={} FLINT_FUZZ_CASE={}\n  ops:\n    {}",
            self.seed,
            self.idx,
            self.seed,
            self.idx,
            self.log.join("\n    ")
        )
    }

    fn file_mode(&mut self) -> u32 {
        loop {
            let m = *self.rng.pick(FILE_MODES);
            // setuid, and setgid with group execute: what a write clears.
            let cleared_by_write = m & 0o4000 != 0 || m & 0o2010 == 0o2010;
            if (self.perms || m & 0o400 != 0) && (!cleared_by_write || known_enabled("suid-lost-on-write")) {
                return m;
            }
        }
    }

    fn dir_mode(&mut self) -> u32 {
        loop {
            let m = *self.rng.pick(DIR_MODES);
            if self.perms || m & 0o500 == 0o500 {
                return m;
            }
        }
    }

    fn body(&mut self) -> Vec<u8> {
        let len = match self.rng.below(100) {
            0..=69 => self.rng.below(64),
            70..=89 => 64 + self.rng.below(4032),
            90..=96 => 4096 + self.rng.below(61440),
            // Past `whole_put_max`: the streaming compose.
            _ => 65536 + self.rng.below(140_000),
        };
        (0..len).map(|_| self.rng.next() as u8).collect()
    }

    /// A name from `UI_NAMES`, less one spelled with U+FFFD while the
    /// collision with a keyless name's lossy form is not generated.
    fn listed_name(&mut self) -> &'static str {
        loop {
            let n = *self.rng.pick(UI_NAMES);
            if !n.contains('\u{fffd}') || known_enabled("lossy-hold-collision") {
                return n;
            }
        }
    }

    fn agent_name(&mut self, at_root: bool) -> Vec<u8> {
        match self.rng.below(100) {
            0..=49 => {
                let n = self.listed_name();
                if at_root && n == CONTROL_DIR { b"a".to_vec() } else { n.as_bytes().to_vec() }
            }
            50..=64 => format!("f{}", self.rng.below(1000)).into_bytes(),
            65..=74 => self.rng.pick(KEYLESS).to_vec(),
            75..=79 => format!("x{}{TMP_SUFFIX}", self.rng.below(10)).into_bytes(),
            80..=84 if !at_root => self.rng.pick(&[STATE_DIR, CONTROL_DIR]).as_bytes().to_vec(),
            85..=89 => {
                if self.rng.pct(50) {
                    "\u{65e5}".repeat(80).into_bytes()
                } else {
                    format!("{}{:05}", "L".repeat(235), self.rng.below(100_000)).into_bytes()
                }
            }
            _ => format!("g{}.txt", self.rng.below(100)).into_bytes(),
        }
    }

    fn ui_name(&mut self, at_root: bool) -> String {
        if self.rng.pct(25) {
            return format!("u{}.txt", self.rng.below(1000));
        }
        let n = self.listed_name();
        if at_root && n == CONTROL_DIR { "u.txt".into() } else { n.to_string() }
    }

    /// The UI's turn, before the agent's: at most one save or delete, of
    /// a path no op of the agent's this tick goes near.
    async fn ui_turn(
        &mut self,
        store: &Arc<MemoryStore>,
        cfg: &LeanConfig,
        s: &Snap,
        d: &Doc,
        last_inj: Option<&Inj>,
    ) -> Vec<Ui> {
        let v = view(s);
        // A clean citation: the tree holds what the document cites, under
        // directories the consume may write.
        let clean: Vec<String> = d
            .iter()
            .filter(|(k, e)| {
                v.seen.get(*k).is_some_and(|f| f.crc == e.crc64_b64 && f.perm == e.mode & 0o7777)
                    && !v.unreadable.contains(*k)
                    && open_dir(s, parent(k.as_bytes()))
                    && path_ok(k)
                    && !last_inj.is_some_and(|i| related(k.as_bytes(), &i.zone) || i.also.iter().any(|a| a == k.as_bytes()))
            })
            .map(|(k, _)| k.clone())
            .collect();
        let op = match self.rng.below(3) {
            0 | 1 if !clean.is_empty() && self.rng.pct(50) => {
                let path = self.rng.pick(&clean).clone();
                if self.rng.pct(65) {
                    let body = self.ui_body();
                    let perm = d[&path].mode & 0o7777;
                    Ui::Write { path, body, perm }
                } else {
                    Ui::Remove { path }
                }
            }
            _ => {
                // A new path, in a directory open to the walk and the
                // consume, maybe one new directory deep.
                let dirs: Vec<Vec<u8>> = std::iter::once(vec![])
                    .chain(s.iter().filter(|(_, n)| matches!(n, Node::Dir { .. })).map(|(p, _)| p.clone()))
                    .filter(|p| {
                        open_dir(s, p)
                            && (p.is_empty() || matches!(vis(s, p), Vis::Seen))
                            && std::str::from_utf8(p).is_ok()
                    })
                    .collect();
                // Nothing in the tree at or under it, and nothing cited at,
                // under or above it.
                let fresh = |q: &[u8]| {
                    !s.contains_key(q)
                        && !s.keys().any(|p| under(p, q))
                        && !d.keys().any(|k| related(k.as_bytes(), q))
                };
                let mut path = String::from_utf8(self.rng.pick(&dirs).clone()).unwrap();
                if self.rng.pct(30) {
                    let sub = self.ui_name(path.is_empty());
                    path = String::from_utf8(join(path.as_bytes(), sub.as_bytes())).unwrap();
                    if !fresh(path.as_bytes()) {
                        return vec![];
                    }
                }
                let name = self.ui_name(path.is_empty());
                let path = String::from_utf8(join(path.as_bytes(), name.as_bytes())).unwrap();
                if !fresh(path.as_bytes()) || !path_ok(&path) || path.len() > self.max_path {
                    return vec![];
                }
                let body = self.ui_body();
                Ui::Write { path, body, perm: 0o644 }
            }
        };
        match &op {
            Ui::Write { path, body, perm } => {
                hitl_write(store, cfg, path, body, "ui").await.unwrap();
                self.note(format!("UI write {path:?} len={} (cited mode {perm:o})", body.len()));
            }
            Ui::Remove { path } => {
                hitl_remove(store, cfg, path, "ui").await;
                self.note(format!("UI remove {path:?}"));
            }
        }
        vec![op]
    }

    fn ui_body(&mut self) -> String {
        let len = self.rng.below(200);
        (0..len).map(|_| (b'a' + self.rng.below(26) as u8) as char).collect()
    }

    /// The agent's turn: one to six operations, none near `frozen`.
    /// Returns the files it wrote or chmodded, the candidates for a
    /// mutation inside the barrier.
    /// `stamped`: per path, the sizes the document and the baseline hold
    /// for it, which a rewrite under a set mtime must not reuse.
    fn agent_turn(&mut self, frozen: &[Vec<u8>], stamped: &BTreeMap<String, Vec<usize>>) -> Vec<Vec<u8>> {
        let mut touched = vec![];
        let n = 1 + self.rng.below(6);
        for _ in 0..n {
            let s = snapshot(&self.root);
            let line = self.agent_op(&s, frozen, stamped, &mut touched);
            if let Some(line) = line {
                self.note(line);
            }
        }
        touched
    }

    fn agent_op(
        &mut self,
        s: &Snap,
        frozen: &[Vec<u8>],
        stamped: &BTreeMap<String, Vec<usize>>,
        touched: &mut Vec<Vec<u8>>,
    ) -> Option<String> {
        let free = |p: &[u8]| !frozen.iter().any(|f| related(p, f));
        let all_dirs: Vec<Vec<u8>> = std::iter::once(vec![])
            .chain(s.iter().filter(|(_, n)| matches!(n, Node::Dir { .. })).map(|(p, _)| p.clone()))
            .collect();
        let open_dirs: Vec<Vec<u8>> = all_dirs.iter().filter(|d| open_dir(s, d)).cloned().collect();
        let files: Vec<Vec<u8>> = s
            .iter()
            .filter(|(p, n)| matches!(n, Node::File { .. }) && searchable(s, p) && free(p))
            .map(|(p, _)| p.clone())
            .collect();
        let root = self.root.clone();
        let w = self.rng.below(100);
        // A new name in an open directory.
        let fresh = |c: &mut Case| -> Option<Vec<u8>> {
            let d = c.rng.pick(&open_dirs).clone();
            let name = c.agent_name(d.is_empty());
            let p = join(&d, &name);
            (!s.contains_key(&p) && free(&p) && p.len() <= c.max_path).then_some(p)
        };
        let res: Result<String, String> = match w {
            // create a file
            0..=29 => {
                let p = fresh(self)?;
                let body = self.body();
                let mode = self.file_mode();
                let a = abs(&root, &p);
                touched.push(p.clone());
                std::fs::write(&a, &body)
                    .and_then(|_| chmod(&a, mode))
                    .map(|_| format!("create {} len={} mode={mode:o}", show(&p), body.len()))
                    .map_err(|e| format!("create {}: {e}", show(&p)))
            }
            // rewrite a file, in place or by rename over it
            30..=44 => {
                let p = self.pick_where(&files, |p| perm_of(s, p) & 0o200 != 0)?;
                let Some(Node::File { body: old, perm, .. }) = s.get(&p) else { unreachable!() };
                let mut body = self.body();
                if self.rng.pct(30) {
                    body.resize(old.len(), 7);
                    body.iter_mut().for_each(|b| *b = self.rng.next() as u8);
                }
                let a = abs(&root, &p);
                touched.push(p.clone());
                if self.rng.pct(30) && open_dir(s, parent(&p)) {
                    let tmp = abs(&root, &[&p[..], b".~new"].concat());
                    std::fs::write(&tmp, &body)
                        .and_then(|_| chmod(&tmp, *perm))
                        .and_then(|_| std::fs::rename(&tmp, &a))
                        .map(|_| format!("rewrite-by-rename {} len={}", show(&p), body.len()))
                        .map_err(|e| format!("rewrite-by-rename {}: {e}", show(&p)))
                } else {
                    std::fs::OpenOptions::new()
                        .write(true)
                        .truncate(true)
                        .open(&a)
                        .and_then(|mut f| std::io::Write::write_all(&mut f, &body))
                        .map(|_| format!("rewrite {} len={} (was {})", show(&p), body.len(), old.len()))
                        .map_err(|e| format!("rewrite {}: {e}", show(&p)))
                }
            }
            // chmod a file
            45..=52 => {
                let p = self.pick_where(&files, |_| true)?;
                let mode = self.file_mode();
                touched.push(p.clone());
                chmod(&abs(&root, &p), mode)
                    .map(|_| format!("chmod {} {mode:o}", show(&p)))
                    .map_err(|e| format!("chmod {}: {e}", show(&p)))
            }
            // delete a file, a link or a special file
            53..=60 => {
                let cands: Vec<Vec<u8>> = s
                    .iter()
                    .filter(|(p, n)| !matches!(n, Node::Dir { .. }) && open_dir(s, parent(p)) && free(p))
                    .map(|(p, _)| p.clone())
                    .collect();
                let p = self.pick_where(&cands, |_| true)?;
                std::fs::remove_file(abs(&root, &p))
                    .map(|_| format!("delete {}", show(&p)))
                    .map_err(|e| format!("delete {}: {e}", show(&p)))
            }
            // make a directory
            61..=70 => {
                let p = fresh(self)?;
                let mode = self.dir_mode();
                let a = abs(&root, &p);
                std::fs::create_dir(&a)
                    .and_then(|_| chmod(&a, mode))
                    .map(|_| format!("mkdir {} mode={mode:o}", show(&p)))
                    .map_err(|e| format!("mkdir {}: {e}", show(&p)))
            }
            // chmod a directory
            71..=76 => {
                let cands: Vec<Vec<u8>> =
                    all_dirs.iter().filter(|d| !d.is_empty() && searchable(s, d) && free(d)).cloned().collect();
                let p = self.pick_where(&cands, |_| true)?;
                let mode = self.dir_mode();
                chmod(&abs(&root, &p), mode)
                    .map(|_| format!("chmod-dir {} {mode:o}", show(&p)))
                    .map_err(|e| format!("chmod-dir {}: {e}", show(&p)))
            }
            // remove a directory and everything under it
            77..=78 => {
                let cands: Vec<Vec<u8>> = all_dirs
                    .iter()
                    .filter(|d| !d.is_empty() && open_dir(s, parent(d)) && free(d))
                    .cloned()
                    .collect();
                let p = self.pick_where(&cands, |_| true)?;
                let a = abs(&root, &p);
                unlock(&a);
                std::fs::remove_dir_all(&a)
                    .map(|_| format!("rmtree {}", show(&p)))
                    .map_err(|e| format!("rmtree {}: {e}", show(&p)))
            }
            // rename a file or directory, maybe over a file
            79..=84 => {
                let cands: Vec<Vec<u8>> = s
                    .keys()
                    .filter(|p| open_dir(s, parent(p)) && free(p))
                    .cloned()
                    .collect();
                let from = self.pick_where(&cands, |_| true)?;
                let d = self.rng.pick(&open_dirs).clone();
                if d == from || under(&d, &from) {
                    return None;
                }
                let name = self.agent_name(d.is_empty());
                let to = join(&d, &name);
                if to.len() > self.max_path || s.keys().any(|q| under(q, &from) && to.len() + q.len() - from.len() > self.max_path) {
                    return None;
                }
                let over_file =
                    matches!(s.get(&to), Some(Node::File { .. })) && matches!(s.get(&from), Some(Node::File { .. }));
                if !free(&to) || to == from || (s.contains_key(&to) && !over_file) {
                    return None;
                }
                if matches!(s.get(&from), Some(Node::File { .. })) {
                    touched.push(to.clone());
                }
                std::fs::rename(abs(&root, &from), abs(&root, &to))
                    .map(|_| format!("rename {} -> {}{}", show(&from), show(&to), if over_file { " (over a file)" } else { "" }))
                    .map_err(|e| format!("rename {} -> {}: {e}", show(&from), show(&to)))
            }
            // replace a file with a symlink, a FIFO, a socket or a directory
            85..=88 => {
                let p = self.pick_where(&files, |p| open_dir(s, parent(p)))?;
                let a = abs(&root, &p);
                let what = self.rng.below(4);
                std::fs::remove_file(&a)
                    .and_then(|_| special(&a, what, &mut self.rng))
                    .map(|k| format!("replace {} with {k}", show(&p)))
                    .map_err(|e| format!("replace {}: {e}", show(&p)))
            }
            // a symlink or special file, new
            89..=91 => {
                let p = fresh(self)?;
                let what = self.rng.below(4);
                special(&abs(&root, &p), what, &mut self.rng)
                    .map(|k| format!("new {k} {}", show(&p)))
                    .map_err(|e| format!("new special {}: {e}", show(&p)))
            }
            // a hard link to a file
            92..=94 => {
                let from = self.pick_where(&files, |_| true)?;
                let to = fresh(self)?;
                touched.push(to.clone());
                std::fs::hard_link(abs(&root, &from), abs(&root, &to))
                    .map(|_| format!("hardlink {} -> {}", show(&to), show(&from)))
                    .map_err(|e| format!("hardlink {} -> {}: {e}", show(&to), show(&from)))
            }
            // a weird mtime, always with a size change (the contract says
            // never to preserve a timestamp across an edit)
            95..=97 => {
                let p = self.pick_where(&files, |p| perm_of(s, p) & 0o200 != 0)?;
                let Some(Node::File { body: old, .. }) = s.get(&p) else { unreachable!() };
                // A size neither the file's now nor one its stamp holds (the
                // document's, the baseline's, which a write inside the
                // barrier leaves apart): what the contract leaves visible
                // with the mtime set. Every name of the file: a hard link's
                // names share the stamp.
                let cited: Vec<usize> = links(s, &p)
                    .iter()
                    .filter_map(|q| std::str::from_utf8(q).ok().and_then(|k| stamped.get(k)))
                    .flatten()
                    .copied()
                    .collect();
                let mut body = self.body();
                while !known_enabled("agent-set-mtime") && (body.len() == old.len() || cited.contains(&body.len())) {
                    body.push(0);
                }
                let (sec, nsec) = *self.rng.pick(&[
                    (0i64, 0i64),
                    (-86_400 * 365, 0),
                    (-1, 500),
                    (1, 999_999_999),
                    (4_102_444_800, 0),
                    (253_402_300_799, 123),
                ]);
                let a = abs(&root, &p);
                touched.push(p.clone());
                std::fs::OpenOptions::new()
                    .write(true)
                    .truncate(true)
                    .open(&a)
                    .and_then(|mut f| std::io::Write::write_all(&mut f, &body))
                    .and_then(|_| set_mtime(&a, sec, nsec))
                    .map(|_| format!("rewrite {} len={} mtime=({sec},{nsec})", show(&p), body.len()))
                    .map_err(|e| format!("weird-mtime {}: {e}", show(&p)))
            }
            // a path near S3's 1024-byte key limit
            _ => {
                let d = self.rng.pick(&open_dirs).clone();
                let target = (880 + self.rng.below(240)).min(self.max_path);
                let mut p = d.clone();
                let mut i = 0;
                while p.len() + 250 < target {
                    p = join(&p, format!("{}{i}", "P".repeat(230)).as_bytes());
                    i += 1;
                }
                let tail = target.saturating_sub(p.len() + 1).clamp(1, 240);
                p = join(&p, "t".repeat(tail).as_bytes());
                // The chain may reuse an earlier one's open directories.
                let blocked = ancestors(&p)
                    .filter(|a| a.len() > d.len())
                    .any(|a| s.get(a).is_some_and(|n| !matches!(n, Node::Dir { .. }) || !open_dir(s, a)));
                if !free(&p) || s.contains_key(&p) || blocked || p.len() > self.max_path {
                    return None;
                }
                let a = abs(&root, &p);
                let body = self.body();
                touched.push(p.clone());
                std::fs::create_dir_all(a.parent().unwrap())
                    .and_then(|_| std::fs::write(&a, &body))
                    .map(|_| format!("longpath {} bytes: {}", p.len(), show(&p)))
                    .map_err(|e| format!("longpath {}: {e}", show(&p)))
            }
        };
        Some(res.unwrap_or_else(|e| format!("(no-op) {e}")))
    }

    fn pick_where(&mut self, xs: &[Vec<u8>], ok: impl Fn(&[u8]) -> bool) -> Option<Vec<u8>> {
        let c: Vec<&Vec<u8>> = xs.iter().filter(|p| ok(p)).collect();
        (!c.is_empty()).then(|| c[self.rng.below(c.len())].clone())
    }

    /// While `file-hold-covers-subtree` is not generated: a file the
    /// syncer may not read, at a path the document still cites something
    /// under, is made readable.
    fn spare_file_holds(&mut self, s: &Snap, d: &Doc) {
        if known_enabled("file-hold-covers-subtree") {
            return;
        }
        for (p, n) in s {
            let Node::File { perm, .. } = n else { continue };
            if perm & 0o400 == 0 && d.keys().any(|k| under(k.as_bytes(), p)) {
                let _ = chmod(&abs(&self.root, p), perm | 0o400);
                self.note(format!("(KNOWN file-hold-covers-subtree not generated: {} made readable)", show(p)));
            }
        }
    }

    /// Maybe arm a mutation of one file this tick wrote, for the moment
    /// between the walk and the open.
    fn arm(&mut self, s: &Snap, d: &Doc, touched: &[Vec<u8>], frozen: &[Vec<u8>]) -> Option<Inj> {
        if !self.rng.pct(40) {
            return None;
        }
        let v = view(s);
        let cands: Vec<Vec<u8>> = touched
            .iter()
            .filter(|p| {
                std::str::from_utf8(p).is_ok_and(|k| v.seen.contains_key(k) && !v.unreadable.contains(k))
                    && !frozen.iter().any(|f| related(p, f) || related(parent(p), f))
            })
            .cloned()
            .collect();
        let x = self.pick_where(&cands, |_| true)?;
        let mut kinds = vec![InjKind::SwapDir, InjKind::SwapLink, InjKind::SwapFifo, InjKind::Grow];
        if known_enabled("enoent-at-open") {
            kinds.push(InjKind::Delete);
        }
        let covers = d.keys().any(|k| under(k.as_bytes(), &x)) && !known_enabled("file-hold-covers-subtree");
        if self.perms && !covers {
            kinds.push(InjKind::Chmod000);
            if !parent(&x).is_empty() {
                kinds.push(InjKind::LockParent);
            }
        }
        let kind = *self.rng.pick(&kinds);
        let zone = if kind == InjKind::LockParent { parent(&x).to_vec() } else { x.clone() };
        let inj = Inj { kind, x: x.clone(), zone, also: links(s, &x) };
        let a = abs(&self.root, &x);
        let pa = abs(&self.root, parent(&x));
        CONSUME_WINDOW_HOOK.with(|h| {
            *h.borrow_mut() = Some((
                String::from_utf8(x).unwrap(),
                "before-upload",
                Box::new(move || {
                    let _ = match kind {
                        InjKind::Delete => std::fs::remove_file(&a),
                        InjKind::Chmod000 => chmod(&a, 0),
                        InjKind::SwapDir => std::fs::remove_file(&a).and_then(|_| std::fs::create_dir(&a)),
                        InjKind::SwapLink => {
                            std::fs::remove_file(&a).and_then(|_| std::os::unix::fs::symlink("/etc/passwd", &a))
                        }
                        InjKind::SwapFifo => {
                            std::fs::remove_file(&a).and_then(|_| special(&a, 1, &mut Rng(0)).map(|_| ()))
                        }
                        InjKind::Grow => std::fs::OpenOptions::new()
                            .append(true)
                            .open(&a)
                            .and_then(|mut f| std::io::Write::write_all(&mut f, &[0x5a; 100])),
                        InjKind::LockParent => chmod(&pa, 0),
                    };
                }),
            ))
        });
        Some(inj)
    }
}

/// Make a symlink (0), FIFO (1), socket (2) or directory (3) at `a`.
fn special(a: &Path, what: usize, rng: &mut Rng) -> std::io::Result<&'static str> {
    let c = CString::new(a.as_os_str().as_bytes()).unwrap();
    let rc = match what {
        0 => {
            let target = *rng.pick(&["/etc/passwd", "../..", ".", "nonexistent", "a"]);
            return std::os::unix::fs::symlink(target, a).map(|_| "symlink");
        }
        1 => unsafe { libc::mkfifo(c.as_ptr(), 0o644) },
        2 => unsafe { libc::mknod(c.as_ptr(), libc::S_IFSOCK | 0o644, 0) },
        _ => return std::fs::create_dir(a).map(|_| "dir"),
    };
    if rc == 0 {
        Ok(if what == 1 { "fifo" } else { "socket" })
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn set_mtime(p: &Path, sec: i64, nsec: i64) -> std::io::Result<()> {
    let c = CString::new(p.as_os_str().as_bytes()).unwrap();
    let ts = [
        libc::timespec { tv_sec: 0, tv_nsec: libc::UTIME_OMIT },
        libc::timespec { tv_sec: sec as libc::time_t, tv_nsec: nsec as _ },
    ];
    if unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), ts.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Give the owner rwx on every directory and rw on every file under `a`
/// (and `a`), keeping every other bit: what `rm -rf` needs, and what the
/// release phase does to the whole tree.
fn unlock(a: &Path) {
    let Ok(m) = std::fs::symlink_metadata(a) else { return };
    let perm = m.permissions().mode() & 0o7777;
    if m.is_dir() {
        let _ = chmod(a, perm | 0o700);
        if let Ok(rd) = std::fs::read_dir(a) {
            for e in rd.flatten() {
                unlock(&e.path());
            }
        }
    } else if m.is_file() {
        let _ = chmod(a, perm | 0o600);
    }
}

// ── the oracle ───────────────────────────────────────────────────────

async fn doc(store: &Arc<MemoryStore>, cfg: &LeanConfig) -> Doc {
    manifest::load(store.as_ref(), cfg).await.unwrap().map(|l| l.manifest.entries).unwrap_or_default()
}

fn brief(r: &BarrierReport) -> String {
    format!(
        "up={:?} del={:?} first_absence={:?} held={:?} deferred={:?} parked={:?} consumed={}",
        r.uploaded, r.deleted, r.first_absence, r.held, r.deferred, r.parked, r.consumed
    )
}

/// Everything one barrier must have done, given the tree before it (`s`)
/// and after it (`after`), the scans this one and the last one made
/// (`v`, `vprev`), the documents before and after, what the UI did and
/// what was mutated inside the barrier.
#[allow(clippy::too_many_arguments)]
fn check(
    r: &BarrierReport,
    dprev: &Doc,
    d: &Doc,
    vprev: &View,
    v: &View,
    s: &Snap,
    after: &Snap,
    ui: &[Ui],
    inj: Option<&Inj>,
) -> Result<(), String> {
    let hit = |p: &[u8]| inj.is_some_and(|i| p == &i.zone[..] || under(p, &i.zone) || i.also.iter().any(|a| a == p));
    let ui_at = |k: &str| ui.iter().find(|u| u.path() == k);

    // PARKED: an upload withheld because the document cites a path it
    // would sit inside or around (`withhold_clashes`) publishes next time.
    for p in &r.parked {
        let clash = dprev.keys().chain(d.keys()).any(|k| under(p.as_bytes(), k.as_bytes()) || under(k.as_bytes(), p.as_bytes()));
        if !clash {
            return Err(format!("PARKED: {p:?} was withheld and nothing cited clashes with it"));
        }
    }

    // PUBLISH
    for (k, f) in &v.seen {
        if v.unreadable.contains(k) || hit(k.as_bytes()) || ui_at(k).is_some() || r.parked.contains(k) {
            continue;
        }
        let Some(e) = d.get(k) else {
            return Err(format!("PUBLISH: {k:?} is in the tree and readable, and not cited"));
        };
        if e.crc64_b64 != f.crc || e.size != f.size || e.mode & 0o7777 != f.perm {
            return Err(format!(
                "PUBLISH: {k:?} is cited as crc {} size {} mode {:o}, the tree has crc {} size {} mode {:o}",
                e.crc64_b64,
                e.size,
                e.mode & 0o7777,
                f.crc,
                f.size,
                f.perm
            ));
        }
    }
    for u in ui {
        match u {
            Ui::Write { path, body, perm } => {
                let Some(e) = d.get(path) else {
                    return Err(format!("UI: the UI's write of {path:?} is not cited after the barrier"));
                };
                if e.crc64_b64 != crc_b64(body.as_bytes()) || e.mode & 0o7777 != *perm {
                    return Err(format!(
                        "UI: {path:?} is cited as crc {} mode {:o}, not as the UI wrote it ({} mode {perm:o})",
                        e.crc64_b64,
                        e.mode & 0o7777,
                        crc_b64(body.as_bytes())
                    ));
                }
                let landed = matches!(after.get(path.as_bytes()),
                    Some(Node::File { body: b, perm: m, .. }) if b == body.as_bytes() && m == perm);
                if !landed {
                    return Err(format!(
                        "UI: the UI's write of {path:?} did not land in the tree as written: {:?}",
                        after.get(path.as_bytes()).map(|n| match n {
                            Node::File { body, perm, .. } => format!("file len {} mode {perm:o}", body.len()),
                            other => format!("{other:?}"),
                        })
                    ));
                }
            }
            Ui::Remove { path } => {
                if d.contains_key(path) {
                    return Err(format!("UI: the UI removed {path:?} and it is cited after the barrier"));
                }
                if after.contains_key(path.as_bytes()) {
                    return Err(format!("UI: the UI removed {path:?} and it is still in the tree"));
                }
            }
        }
    }

    // HELD
    for (k, e) in dprev {
        if ui_at(k).is_some() || hit(k.as_bytes()) {
            continue;
        }
        if v.covered(k) || v.unreadable.contains(k) {
            if d.get(k).map(|n| &n.etag) != Some(&e.etag) {
                return Err(format!(
                    "HELD: {k:?} is held (under {:?} or unreadable), and its citation moved: {} -> {:?}",
                    v.held_at.iter().find(|h| k.as_bytes() == &h[..] || under(k.as_bytes(), h)).map(|h| show(h)),
                    e.etag,
                    d.get(k).map(|n| &n.etag)
                ));
            }
        }
    }

    // NO EARLY DELETE
    for k in dprev.keys() {
        if d.contains_key(k) || matches!(ui_at(k), Some(Ui::Remove { .. })) {
            continue;
        }
        if vprev.present(k) || v.present(k) {
            return Err(format!(
                "NO EARLY DELETE: {k:?} was deleted, and was present in {} scan",
                if v.present(k) { "this" } else { "the last" }
            ));
        }
    }

    // NO STALE CITATION
    for (k, e) in d {
        let moved = dprev.get(k).map(|p| p.etag != e.etag || p.mode != e.mode).unwrap_or(true);
        if moved {
            if !v.seen.contains_key(k) && !matches!(ui_at(k), Some(Ui::Write { .. })) {
                return Err(format!("NO STALE CITATION: {k:?} was newly cited and this scan never saw it"));
            }
        } else if !v.present(k) && !vprev.present(k) {
            return Err(format!("NO STALE CITATION: {k:?} is absent from two scans and still cited"));
        }
    }

    // HONEST HOLDS
    let held: BTreeSet<&String> = r.held.iter().collect();
    for h in &held {
        if !v.held.contains(*h) && !v.unreadable.contains(*h) && !hit(h.as_bytes()) {
            return Err(format!("HONEST HOLDS: {h:?} was held, and the walk could read and name it"));
        }
    }
    for h in &v.held {
        if !held.contains(h) {
            return Err(format!("HONEST HOLDS: {h:?} cannot be read or named, and was not held"));
        }
    }
    for h in &v.unreadable {
        if !hit(h.as_bytes()) && !held.contains(h) {
            return Err(format!("HONEST HOLDS: {h:?} is a file the syncer may not read, and was not held"));
        }
    }

    // TREE INTEGRITY
    let ui_dirs = |p: &[u8]| ui.iter().any(|u| matches!(u, Ui::Write { .. }) && under(u.path().as_bytes(), p));
    for p in s.keys().chain(after.keys()) {
        if hit(p) || std::str::from_utf8(p).is_ok_and(|k| ui_at(k).is_some()) {
            continue;
        }
        let (was, now) = (s.get(p), after.get(p));
        if was == now || (was.is_none() && matches!(now, Some(Node::Dir { .. })) && ui_dirs(p)) {
            continue;
        }
        let desc = |n: Option<&Node>| match n {
            Some(Node::File { body, perm, .. }) => format!("file len {} crc {} mode {perm:o}", body.len(), crc_b64(body)),
            other => format!("{other:?}"),
        };
        return Err(format!("TREE INTEGRITY: the barrier changed {}: {} -> {}", show(p), desc(was), desc(now)));
    }
    Ok(())
}

// ── the run ──────────────────────────────────────────────────────────

/// A wedged barrier (an open that blocks, a loop) must not hang the
/// battery silently: past this long without progress, print what the
/// case was doing and abort.
const WATCHDOG_SECS: u64 = 300;
static HEARTBEAT: AtomicU64 = AtomicU64::new(0);
static DOING: Mutex<String> = Mutex::new(String::new());

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

fn beat(doing: impl FnOnce() -> String) {
    HEARTBEAT.store(now_secs(), Ordering::SeqCst);
    *DOING.lock().unwrap() = doing();
}

/// A temp tree the fuzzer may have locked: unlocked before it is
/// removed, or a non-root `remove_dir_all` leaves it behind.
struct Tree(tempfile::TempDir);

impl Drop for Tree {
    fn drop(&mut self) {
        unlock(self.0.path());
    }
}

async fn fuzz_syncer(store: &Arc<MemoryStore>, root: &Path, threads: usize) -> Syncer {
    let mut sc = syncer(store, root).await;
    // The streaming compose from 64 KiB, so the generator reaches it.
    sc.cfg.whole_put_max = 64 * 1024;
    sc.cfg.scan_threads = threads;
    sc
}

#[derive(Default, Debug)]
struct Tally {
    barriers: usize,
    uploads: usize,
    held: usize,
    fired: usize,
    ui: usize,
}

async fn run_case(seed: u64, idx: usize, ticks: usize, perms: bool) -> Result<Tally, String> {
    let mut rng = Rng(seed ^ (idx as u64 + 1).wrapping_mul(0xA24B_AED4_963E_E407));
    let threads = *rng.pick(&[1usize, 2, 8]);
    let tree_a = Tree(tempfile::tempdir().unwrap());
    let root = tree_a.0.path().to_path_buf();
    let max_path = if known_enabled("long-keys") { usize::MAX } else { SAFE_PATH };
    let mut c = Case { seed, idx, rng, root: root.clone(), perms, max_path, log: vec![format!("walk threads {threads}")] };
    let store = Arc::new(MemoryStore::new());
    let mut a = fuzz_syncer(&store, &root, threads).await;
    a.checkout().await.map_err(|e| c.fail("checkout A", format!("{e}")))?;
    let mut t = Tally::default();
    let mut dprev = doc(&store, &a.cfg).await;
    let mut vprev = View::default();
    let mut last_inj: Option<Inj> = None;
    let mut s0 = snapshot(&root);

    // One barrier, checked: returns the report, the scan's view and the
    // tree after.
    macro_rules! barrier {
        ($at:expr, $ui:expr, $inj:expr) => {{
            let at: String = $at;
            beat(|| format!("seed={} case={} {at}", c.seed, c.idx));
            let s = snapshot(&root);
            let mut v = view(&s);
            for u in $ui.iter() {
                match u {
                    Ui::Write { path, body, perm } => {
                        v.seen.insert(
                            path.clone(),
                            Seen { crc: crc_b64(body.as_bytes()), size: body.len() as u64, perm: *perm },
                        );
                    }
                    Ui::Remove { path } => {
                        v.seen.remove(path);
                    }
                }
            }
            let inj: Option<Inj> = $inj;
            let r = a.run_barrier().await;
            let fired = CONSUME_WINDOW_HOOK.with(|h| h.borrow_mut().take().is_none());
            let inj = inj.filter(|_| fired);
            if let Some(i) = &inj {
                c.note(format!("  injected {:?} at {} inside the barrier", i.kind, show(&i.x)));
                t.fired += 1;
            }
            let after = snapshot(&root);
            let d = doc(&store, &a.cfg).await;
            let r = r.map_err(|e| c.fail(&at, format!("LIVENESS: the barrier failed: {e}")))?;
            c.note(format!("  {at}: {}", brief(&r)));
            t.barriers += 1;
            t.uploads += r.uploaded.len();
            t.held += r.held.len();
            check(&r, &dprev, &d, &vprev, &v, &s, &after, &$ui, inj.as_ref()).map_err(|e| c.fail(&at, e))?;
            dprev = d;
            vprev = v;
            (r, after, inj)
        }};
    }

    for tick in 0..ticks {
        c.note(format!("tick {tick}"));
        let ui = if c.rng.pct(30) {
            c.ui_turn(&store, &a.cfg, &s0, &dprev, last_inj.as_ref()).await
        } else {
            vec![]
        };
        t.ui += ui.len();
        let frozen: Vec<Vec<u8>> = ui.iter().flat_map(|u| links(&s0, u.path().as_bytes())).collect();
        let mut stamped: BTreeMap<String, Vec<usize>> =
            dprev.iter().map(|(k, e)| (k.clone(), vec![e.size as usize])).collect();
        for (k, b) in a.state.load_baseline().unwrap().entries.iter() {
            stamped.entry(k.clone()).or_default().push(b.size as usize);
        }
        let touched = c.agent_turn(&frozen, &stamped);
        c.spare_file_holds(&snapshot(&root), &dprev);
        let s = snapshot(&root);
        let inj = c.arm(&s, &dprev, &touched, &frozen);
        let (_, after, inj) = barrier!(format!("tick {tick} barrier"), ui, inj);
        s0 = after;
        last_inj = inj;
    }

    // Quiet: nothing more from anyone. The third barrier has nothing to do.
    for q in 0..3 {
        let (r, _, _) = barrier!(format!("quiet barrier {q}"), Vec::<Ui>::new(), None);
        if q == 2 && !(r.uploaded.is_empty() && r.deleted.is_empty() && r.first_absence.is_empty() && r.deferred.is_empty() && r.consumed == 0) {
            return Err(c.fail("quiet barrier 2", format!("QUIET: a third idle barrier still did work: {}", brief(&r))));
        }
    }

    // Released: everything readable. The document is the tree.
    c.note("release: owner rwx on every directory, rw on every file".into());
    unlock(&root);
    let mut released = View::default();
    for q in 0..3 {
        let (r, after, _) = barrier!(format!("released barrier {q}"), Vec::<Ui>::new(), None);
        if q == 2 {
            if !(r.uploaded.is_empty() && r.deleted.is_empty() && r.first_absence.is_empty() && r.deferred.is_empty()) {
                return Err(c.fail("released barrier 2", format!("QUIET: a third idle barrier still did work: {}", brief(&r))));
            }
            released = view(&after);
        }
    }
    let cited: BTreeSet<&String> = dprev.keys().collect();
    let seen: BTreeSet<&String> = released.seen.keys().collect();
    if cited != seen {
        return Err(c.fail(
            "released",
            format!(
                "DOCUMENT = TREE: cited and not in the tree {:?}; in the tree and not cited {:?}",
                cited.difference(&seen).collect::<Vec<_>>(),
                seen.difference(&cited).collect::<Vec<_>>()
            ),
        ));
    }

    // A fresh checkout is the document, and publishes nothing.
    beat(|| format!("seed={seed} case={idx} checkout B"));
    let tree_b = Tree(tempfile::tempdir().unwrap());
    let mut b = fuzz_syncer(&store, tree_b.0.path(), threads).await;
    b.checkout().await.map_err(|e| c.fail("checkout B", format!("CHECKOUT: {e}")))?;
    let sb = snapshot(tree_b.0.path());
    for (p, n) in &sb {
        match n {
            Node::Dir { .. } => {}
            Node::File { body, perm, .. } => {
                let k = std::str::from_utf8(p).map_err(|_| c.fail("checkout B", format!("CHECKOUT: made {}", show(p))))?;
                let Some(e) = dprev.get(k) else {
                    return Err(c.fail("checkout B", format!("CHECKOUT: made {k:?}, which is not cited")));
                };
                if e.crc64_b64 != crc_b64(body) || e.mode & 0o7777 != *perm {
                    return Err(c.fail(
                        "checkout B",
                        format!("CHECKOUT: {k:?} is crc {} mode {perm:o}, cited crc {} mode {:o}", crc_b64(body), e.crc64_b64, e.mode & 0o7777),
                    ));
                }
            }
            other => return Err(c.fail("checkout B", format!("CHECKOUT: made {} as {other:?}", show(p)))),
        }
    }
    for k in dprev.keys() {
        if !matches!(sb.get(k.as_bytes()), Some(Node::File { .. })) {
            return Err(c.fail("checkout B", format!("CHECKOUT: {k:?} is cited and was not checked out")));
        }
    }
    for q in 0..2 {
        let r = b.run_barrier().await.map_err(|e| c.fail(&format!("B barrier {q}"), format!("LIVENESS: {e}")))?;
        if !(r.uploaded.is_empty() && r.deleted.is_empty() && r.first_absence.is_empty() && r.held.is_empty()) {
            return Err(c.fail(&format!("B barrier {q}"), format!("CHECKOUT: a fresh checkout published: {}", brief(&r))));
        }
    }

    // S3 refuses a key longer than 1024 bytes.
    let long: Vec<String> = store
        .list(&a.cfg.prefix)
        .await
        .unwrap()
        .into_iter()
        .map(|o| o.key)
        .filter(|k| k.len() > 1024)
        .collect();
    if !long.is_empty() {
        return Err(c.fail(
            "end",
            format!("KEY LENGTH: {} store keys are longer than S3's 1024 bytes, the first {} bytes", long.len(), long[0].len()),
        ));
    }
    Ok(t)
}

#[tokio::test]
async fn hostile_trees_publish_what_the_contract_says() {
    let env = |k: &str| std::env::var(k).ok();
    let seed: u64 = match env("FLINT_FUZZ_SEED").as_deref() {
        None => 0x5EED_F1A7,
        Some("random") => now_secs() ^ u64::from(std::process::id()) << 32,
        Some(s) => s.parse().expect("FLINT_FUZZ_SEED is a number or `random`"),
    };
    let cases: usize = env("FLINT_FUZZ_CASES").and_then(|v| v.parse().ok()).unwrap_or(12);
    let ticks: usize = env("FLINT_FUZZ_TICKS").and_then(|v| v.parse().ok()).unwrap_or(12);
    let only: Option<usize> = env("FLINT_FUZZ_CASE").and_then(|v| v.parse().ok());
    let perms = unsafe { libc::geteuid() } != 0;
    if !perms {
        eprintln!("SKIPPED: running as root, mode bits cannot induce EACCES: the fuzzer's permission half is off");
    }
    for k in KNOWN {
        if !known_enabled(k.name) {
            eprintln!("KNOWN {}: not generated ({}); FLINT_FUZZ_KNOWN={} generates it", k.name, k.what, k.name);
        }
    }
    HEARTBEAT.store(now_secs(), Ordering::SeqCst);
    let watchdog_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let done = watchdog_done.clone();
    std::thread::spawn(move || {
        while !done.load(Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_secs(5));
            if now_secs().saturating_sub(HEARTBEAT.load(Ordering::SeqCst)) > WATCHDOG_SECS {
                eprintln!("FUZZ WATCHDOG: no progress in {WATCHDOG_SECS} s at {}", DOING.lock().unwrap());
                std::process::abort();
            }
        }
    });

    let mut failures = vec![];
    for idx in (0..cases).filter(|i| only.is_none_or(|o| o == *i)) {
        match run_case(seed, idx, ticks, perms).await {
            Ok(t) => eprintln!("fuzz seed={seed} case={idx}: ok {t:?}"),
            Err(e) => {
                eprintln!("{e}\n");
                failures.push(format!("case {idx}: {}", e.lines().next().unwrap_or_default()));
            }
        }
    }
    watchdog_done.store(true, Ordering::SeqCst);
    assert!(failures.is_empty(), "{} of {cases} hostile-tree cases failed (seed {seed}):\n{}", failures.len(), failures.join("\n"));
}

/// The tree changes WHILE the barrier walks it: a thread makes and removes
/// files and directories in a large directory as fast as it can. A name
/// the walk listed and then could not stat or open is simply gone; that is
/// the two-scan rule's business, and must never fail the barrier.
#[tokio::test]
async fn a_tree_churning_under_the_walk_does_not_fail_the_barrier() {
    if !known_enabled("enoent-at-open") {
        eprintln!("KNOWN enoent-at-open: the churn test is not run; FLINT_FUZZ_KNOWN=enoent-at-open runs it");
        return;
    }
    use std::sync::atomic::AtomicBool;
    let store = Arc::new(MemoryStore::new());
    // FLINT_FUZZ_CHURN_DIR puts the tree on another filesystem than /tmp's.
    let tree = Tree(match std::env::var("FLINT_FUZZ_CHURN_DIR") {
        Ok(d) => tempfile::tempdir_in(d).unwrap(),
        Err(_) => tempfile::tempdir().unwrap(),
    });
    let root = tree.0.path().to_path_buf();
    std::fs::create_dir(root.join("d")).unwrap();
    for i in 0..4000 {
        std::fs::write(root.join(format!("d/f{i}")), format!("{i}")).unwrap();
    }
    let mut a = fuzz_syncer(&store, &root, 1).await;
    a.checkout().await.unwrap();
    a.run_barrier().await.unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let (st, r) = (stop.clone(), root.clone());
    let churn = std::thread::spawn(move || {
        let mut n = 0u64;
        // A sliding window: each name lives `live` rounds, long enough to
        // be listed, and goes at a moment the walk does not choose.
        // FLINT_FUZZ_CHURN_RATE paces it (rounds a second; a round makes a
        // file and a directory and removes an old pair); unpaced by default.
        let rate: u64 = std::env::var("FLINT_FUZZ_CHURN_RATE").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
        let live: u64 = std::env::var("FLINT_FUZZ_CHURN_LIVE").ok().and_then(|v| v.parse().ok()).unwrap_or(500);
        let t0 = std::time::Instant::now();
        while !st.load(Ordering::Relaxed) {
            let _ = std::fs::write(r.join(format!("d/c{n}")), b"x");
            let sub = r.join(format!("d/s{n}"));
            let _ = std::fs::create_dir(&sub).and_then(|_| std::fs::write(sub.join("in"), b"y"));
            if n >= live {
                let _ = std::fs::remove_file(r.join(format!("d/c{}", n - live)));
                let old = r.join(format!("d/s{}", n - live));
                let _ = std::fs::remove_file(old.join("in")).and_then(|_| std::fs::remove_dir(&old));
            }
            n += 1;
            if rate > 0 {
                let due = std::time::Duration::from_micros(n * 1_000_000 / rate);
                if let Some(wait) = due.checked_sub(t0.elapsed()) {
                    std::thread::sleep(wait);
                }
            }
        }
        n
    });
    let mut errors: BTreeMap<String, usize> = BTreeMap::new();
    let barriers = 60;
    let mut did = Tally::default();
    for _ in 0..barriers {
        let r = a.run_barrier().await;
        if let Ok(r) = &r {
            did.barriers += 1;
            did.uploads += r.uploaded.len();
            did.fired += r.deferred.len() + r.first_absence.len() + r.deleted.len();
        }
        if let Err(e) = r {
            // The path in the message varies; the error's shape does not.
            let shape = e.to_string().replace(root.to_str().unwrap(), "<root>");
            let shape = shape.split(|c: char| c.is_ascii_digit()).collect::<Vec<_>>().join("#");
            *errors.entry(shape).or_default() += 1;
        }
    }
    stop.store(true, Ordering::Relaxed);
    let churned = churn.join().unwrap();
    eprintln!(
        "churn: {churned} rounds during {barriers} barriers; {} Ok, {} uploads, {} deferred/absent/deleted; failed barriers by error: {errors:#?}",
        did.barriers, did.uploads, did.fired
    );
    assert!(errors.is_empty(), "{} of {barriers} barriers failed while the tree churned: {errors:#?}", errors.values().sum::<usize>());
}

