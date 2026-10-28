//! Identity events — L2 step 6 (design review A7).
//!
//! RENAME and REMOVE change what a file's bucket key SHOULD be, or
//! whether its object should exist at all. Their durable halves
//! (tombstones, row re-keying, the covered file's atomic tombstone)
//! ride the SAME pre-ack discipline as dirty marks: the handler queues
//! an event synchronously, and the dispatcher's drain applies it to
//! the backend BEFORE the op's reply exists — a rename whose tombstone
//! could vanish in a crash is the same bug class as an acked write
//! whose dirty bit could (C1).
//!
//! Events apply IN ORDER (a Vec, not a map): rename chains and
//! remove-then-recreate sequences are order-sensitive. A failed apply
//! requeues the REMAINDER at the front and surfaces the error so the
//! dispatcher refuses the ack.
//!
//! Without this module, git's tmp-write+rename idiom — the tier's
//! proof workload — produces a false foreign-overwrite wedge on every
//! object finalize.
//!
//! An event names its dead file by `(dev, ino)`, which identifies a
//! file only while its inode lives — so the handler pins the inode
//! before the unlink and the pin rides in the event ([`InodePin`]).

use crate::state_backend::{StateBackend, StateBackendResult};
use crate::tier::capture;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tracing::debug;

#[derive(Debug, Clone)]
pub enum Event {
    Renamed {
        /// The moved file (post-rename identity) — None if the stat
        /// raced away (directory renames carry no tier rows).
        moved: Option<(u64, u64)>,
        new_path: PathBuf,
        /// Mark sequence for the moved file's dirty-bit upsert.
        seq: u64,
        /// The replaced destination file, if the rename covered one.
        covered: Option<(u64, u64)>,
        /// Holds `covered`'s inode until this event has applied.
        covered_pin: InodePin,
    },
    Removed {
        ident: (u64, u64),
        /// Holds `ident`'s inode until this event has applied.
        pin: InodePin,
    },
}

/// Keeps a dead file's inode allocated until the event that names it
/// has applied.
///
/// REMOVE frees the inode at the unlink, and a rename-over frees the
/// covered file's at the rename, but the event applies later: at a
/// drain, after the parent's commit, and again after a failed apply.
/// A CREATE in that window can be handed the same number (ext4 hands a
/// freed inode straight back) and mark its new file dirty. The drain
/// then deletes the new file's dirty row and forgets its capture state
/// and eviction marker, and an acked file is never uploaded. An
/// `O_PATH` descriptor opened before the unlink holds the inode, so no
/// file can take its number while the event is queued, requeued or
/// applying; it closes when the applied event drops, after the RAM
/// forgets.
///
/// Empty off Linux (the tier runs on Linux), and empty when the open
/// fails: the event then applies unpinned, as before pins existed.
#[derive(Debug, Clone, Default)]
pub struct InodePin(Option<Arc<std::fs::File>>);

impl InodePin {
    /// Pin the regular file at `path`, never following a final
    /// symlink, if it is still the inode `ident` names.
    pub fn of(path: &Path, ident: (u64, u64)) -> Self {
        #[cfg(target_os = "linux")]
        let held = {
            use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_PATH | libc::O_NOFOLLOW)
                .open(path)
                .ok()
                .filter(|f| {
                    f.metadata()
                        .is_ok_and(|m| m.is_file() && (m.dev(), m.ino()) == ident)
                })
        };
        #[cfg(not(target_os = "linux"))]
        let held = {
            let _ = (path, ident);
            None
        };
        Self(held.map(Arc::new))
    }

    #[cfg(test)]
    fn file(&self) -> Option<&Arc<std::fs::File>> {
        self.0.as_ref()
    }
}

/// Tests: is an event naming the dead file `dead` (removed, or covered
/// by a rename) still queued, and what does its pin hold? `None`: no
/// queued event names it (applied, or a parallel test's drain took
/// it). `Some(None)`: queued unpinned.
#[cfg(test)]
pub fn queued_pin_of(dead: (u64, u64)) -> Option<Option<std::fs::Metadata>> {
    let q = events().lock().unwrap();
    q.iter().find_map(|ev| {
        let (id, pin) = match ev {
            Event::Removed { ident, pin } => (Some(*ident), pin),
            Event::Renamed { covered, covered_pin, .. } => (*covered, covered_pin),
        };
        (id == Some(dead)).then(|| pin.file().and_then(|f| f.metadata().ok()))
    })
}

static EVENTS: OnceLock<Mutex<Vec<Event>>> = OnceLock::new();
static HAS_EVENTS: AtomicBool = AtomicBool::new(false);

fn events() -> &'static Mutex<Vec<Event>> {
    EVENTS.get_or_init(|| Mutex::new(Vec::new()))
}

/// RENAME hook (fileops, post-success). Cheap and sync; no-op with the
/// tier off.
/// `covered_pin` holds the covered file's inode, taken before the
/// rename ([`InodePin`]).
pub fn note_rename(
    moved: Option<(u64, u64)>,
    new_path: &Path,
    covered: Option<(u64, u64)>,
    covered_pin: InodePin,
) {
    if !capture::enabled() {
        return;
    }
    let ev = Event::Renamed {
        moved,
        new_path: new_path.to_path_buf(),
        seq: capture::next_mark_seq(),
        covered,
        covered_pin,
    };
    events().lock().unwrap().push(ev);
    HAS_EVENTS.store(true, Ordering::Release);
}

/// REMOVE hook (fileops, post-success, regular files only).
/// `links_remaining` is the inode's link count AFTER this unlink, and
/// `pin` holds the inode, taken before it ([`InodePin`]).
///
/// It is a parameter, not something this module can derive: by the time
/// the event is queued the name is already gone, and `(dev, ino)` alone
/// cannot say whether the inode still answers to another one.
///
/// A tombstone deletes the inode's OBJECT and drops its generation row.
/// That is right when the last name goes and catastrophic when it does
/// not — the surviving name loses its only published copy, and if the
/// inode was evicted it becomes a permanent 0-byte file that READs as
/// EOF under NFS4_OK. `LINK` is refused while the tier is on, but the
/// refusal does not cover a hard-linked pair adopted by import, or one
/// that predates the tier being switched on, and neither does it cover
/// a pair created through any other door onto the same export.
pub fn note_remove(ident: (u64, u64), links_remaining: u64, pin: InodePin) {
    if !capture::enabled() {
        return;
    }
    if links_remaining > 0 {
        debug!(
            "tier identity: {:?} keeps {} more name(s) — no tombstone, the object is \
             still referenced",
            ident, links_remaining
        );
        return;
    }
    events().lock().unwrap().push(Event::Removed { ident, pin });
    HAS_EVENTS.store(true, Ordering::Release);
}

/// Cheap per-op check for the dispatcher (pairs with
/// `capture::has_pending`).
#[inline]
pub fn has_pending() -> bool {
    HAS_EVENTS.load(Ordering::Acquire)
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Apply every queued event to the backend, in order. On failure the
/// unapplied remainder goes back to the FRONT (order preserved) and
/// the error surfaces for the dispatcher's ack refusal.
pub async fn drain(backend: &Arc<dyn StateBackend>) -> StateBackendResult<()> {
    let taken: Vec<Event> = {
        let mut q = events().lock().unwrap();
        HAS_EVENTS.store(false, Ordering::Release);
        std::mem::take(&mut *q)
    };
    if taken.is_empty() {
        return Ok(());
    }
    let ts = now_unix();
    for (i, ev) in taken.iter().enumerate() {
        let res = match ev {
            Event::Renamed { moved, new_path, seq, covered, .. } => {
                backend
                    .tier_apply_rename(
                        *moved,
                        &new_path.to_string_lossy(),
                        *seq,
                        *covered,
                        ts,
                    )
                    .await
            }
            Event::Removed { ident, .. } => backend.tier_apply_remove(*ident, ts).await,
        };
        match res {
            Ok(()) => {
                // The dead identity's RAM traces go with the applied
                // event, never before (the tx could fail). Eviction
                // markers included (step 10): the backend tx already
                // deleted the durable row for covered/removed files.
                match ev {
                    Event::Renamed { covered: Some(c), .. } => {
                        capture::forget(c.0, c.1);
                        crate::tier::evict::forget(c.0, c.1);
                    }
                    Event::Removed { ident, .. } => {
                        capture::forget(ident.0, ident.1);
                        crate::tier::evict::forget(ident.0, ident.1);
                    }
                    _ => {}
                }
            }
            Err(e) => {
                // The requeued events keep their pins.
                let mut q = events().lock().unwrap();
                let mut rest: Vec<Event> = taken[i..].to_vec();
                rest.append(&mut q);
                *q = rest;
                HAS_EVENTS.store(true, Ordering::Release);
                return Err(e);
            }
        }
    }
    // `taken` drops here, and the applied events' pins with it: only
    // now, after every forget above, may a dead number be reused.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state_backend::memory::MemoryBackend;
    use crate::state_backend::{TierGenerationRow, TierTombstone};

    fn gen_row(dev: u64, ino: u64, key: &str) -> TierGenerationRow {
        TierGenerationRow {
            dev,
            ino,
            key: key.into(),
            generation: 3,
            etag: format!("\"etag-{}\"", ino),
            crc64_b64: None,
            size: 10,
            copy_allowed: true,
            updated_unix: 1,
        }
    }

    /// Rename-over: the covered file's generation becomes a tombstone
    /// ATOMICALLY; the moved file's bit re-points at the new path.
    /// (Events are process-global like capture's queue — a parallel
    /// test's drain may apply OUR event into ITS backend; the repair
    /// loop re-notes until our backend shows the result.)
    #[tokio::test]
    async fn rename_over_tombstones_covered_and_repoints_moved() {
        // Queues and/or drains the PROCESS-GLOBAL capture queue.
        // Held for the whole body: the theft window is queue-to-drain,
        // not the drain alone. See `capture::test_exclusive`.
        let _excl = crate::tier::capture::test_exclusive();
        // A dropped TempDir hands its inodes to the next one, and on
        // ext4 that reuse is deterministic — so start from no
        // process-global capture state at all. See reset_for_tests.
        crate::tier::capture::reset_for_tests();
        crate::tier::evict::reset_for_tests();
        capture::force_enable();
        let be: Arc<dyn StateBackend> = Arc::new(MemoryBackend::new());
        let (dev, moved_ino, covered_ino) = (0x1D7_u64, 0xA1_u64, 0xA2_u64);
        be.tier_upsert_generation(&gen_row(dev, covered_ino, "t/final")).await.unwrap();

        for _ in 0..50 {
            note_rename(
                Some((dev, moved_ino)),
                Path::new("/exp/final"),
                Some((dev, covered_ino)),
                InodePin::default(),
            );
            let _ = drain(&be).await;
            let done = be
                .tier_list_tombstones()
                .await
                .unwrap()
                .iter()
                .any(|t| t.key == "t/final");
            if done {
                break;
            }
            // theft-repair: re-seed the covered row and try again
            be.tier_upsert_generation(&gen_row(dev, covered_ino, "t/final")).await.unwrap();
        }

        let tombs = be.tier_list_tombstones().await.unwrap();
        let t = tombs.iter().find(|t| t.key == "t/final").expect("covered must tombstone");
        assert_eq!(t.etag.as_deref(), Some(format!("\"etag-{}\"", covered_ino).as_str()));
        assert!(
            be.tier_list_generations().await.unwrap().iter().all(|g| g.ino != covered_ino),
            "the covered generation row must be gone"
        );
        let dirty = be.tier_list_dirty().await.unwrap();
        let m = dirty
            .iter()
            .find(|r| r.dev == dev && r.ino == moved_ino)
            .expect("moved file must be bit-set for the re-key flush");
        assert_eq!(m.path.as_deref(), Some("/exp/final"), "path must OVERWRITE");
    }

    #[tokio::test]
    async fn remove_tombstones_and_forgets() {
        // Queues and/or drains the PROCESS-GLOBAL capture queue.
        // Held for the whole body: the theft window is queue-to-drain,
        // not the drain alone. See `capture::test_exclusive`.
        let _excl = crate::tier::capture::test_exclusive();
        // A dropped TempDir hands its inodes to the next one, and on
        // ext4 that reuse is deterministic — so start from no
        // process-global capture state at all. See reset_for_tests.
        crate::tier::capture::reset_for_tests();
        crate::tier::evict::reset_for_tests();
        capture::force_enable();
        let be: Arc<dyn StateBackend> = Arc::new(MemoryBackend::new());
        let (dev, ino) = (0x1D7_u64, 0xB1_u64);

        for _ in 0..50 {
            be.tier_upsert_generation(&gen_row(dev, ino, "t/victim")).await.unwrap();
            capture::note(dev, ino, capture::Mutation::Write { offset: 0, len: 4 });
            note_remove((dev, ino), 0, InodePin::default());
            let _ = drain(&be).await;
            if be.tier_list_tombstones().await.unwrap().iter().any(|t| t.key == "t/victim") {
                break;
            }
        }
        assert!(be
            .tier_list_tombstones()
            .await
            .unwrap()
            .iter()
            .any(|t| t.key == "t/victim"));
        assert!(be.tier_list_generations().await.unwrap().iter().all(|g| g.ino != ino));
        assert!(
            be.tier_list_dirty().await.unwrap().iter().all(|r| !(r.dev == dev && r.ino == ino)),
            "a removed file's dirty bit must go with it"
        );
        assert!(capture::snapshot(dev, ino).is_none(), "capture must forget the dead identity");
        assert!(!capture::is_durable(dev, ino));
    }

    /// The pin holds the unlinked file's inode, and the applied event
    /// lets it go. A parallel test's drain may apply our event instead
    /// of ours; whoever applies it drops the pin, so wait for that.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_pin_holds_the_dead_inode_until_its_event_applies() {
        use std::os::unix::fs::MetadataExt;
        let _excl = crate::tier::capture::test_exclusive();
        crate::tier::capture::reset_for_tests();
        crate::tier::evict::reset_for_tests();
        capture::force_enable();
        let be: Arc<dyn StateBackend> = Arc::new(MemoryBackend::new());
        let temp = tempfile::TempDir::new().unwrap();
        let f = temp.path().join("victim.bin");
        std::fs::write(&f, b"x").unwrap();
        let md = std::fs::metadata(&f).unwrap();
        let ident = (md.dev(), md.ino());

        let link = temp.path().join("link");
        std::os::unix::fs::symlink(&f, &link).unwrap();
        assert!(InodePin::of(&link, ident).file().is_none(), "a symlink is never pinned");
        assert!(
            InodePin::of(&f, (ident.0, ident.1 + 1)).file().is_none(),
            "a pin holds only the inode its event names"
        );

        let pin = InodePin::of(&f, ident);
        let weak = Arc::downgrade(pin.file().expect("a regular file is pinned"));
        std::fs::remove_file(&f).unwrap();
        let held = pin.file().unwrap().metadata().unwrap();
        assert_eq!((held.dev(), held.ino(), held.nlink()), (ident.0, ident.1, 0));

        note_remove(ident, 0, pin);
        let _ = drain(&be).await;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while weak.upgrade().is_some() {
            assert!(
                std::time::Instant::now() < deadline,
                "the applied event must release its pin"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// The tombstone type itself round-trips through put/list/delete
    /// (sqlite's reopen coverage lives in the sqlite tests).
    #[tokio::test]
    async fn tombstone_surface_roundtrip() {
        let be: Arc<dyn StateBackend> = Arc::new(MemoryBackend::new());
        be.tier_put_tombstone(&TierTombstone {
            key: "t/x".into(),
            etag: None,
            created_unix: 9,
        })
        .await
        .unwrap();
        assert_eq!(be.tier_list_tombstones().await.unwrap().len(), 1);
        be.tier_delete_tombstone("t/x").await.unwrap();
        assert!(be.tier_list_tombstones().await.unwrap().is_empty());
    }
}
