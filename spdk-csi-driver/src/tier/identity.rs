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

use crate::state_backend::{StateBackend, StateBackendResult};
use crate::tier::capture;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

#[derive(Debug, Clone, PartialEq, Eq)]
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
    },
    Removed {
        ident: (u64, u64),
    },
}

static EVENTS: OnceLock<Mutex<Vec<Event>>> = OnceLock::new();
static HAS_EVENTS: AtomicBool = AtomicBool::new(false);

fn events() -> &'static Mutex<Vec<Event>> {
    EVENTS.get_or_init(|| Mutex::new(Vec::new()))
}

/// RENAME hook (fileops, post-success). Cheap and sync; no-op with the
/// tier off.
pub fn note_rename(
    moved: Option<(u64, u64)>,
    new_path: &Path,
    covered: Option<(u64, u64)>,
) {
    if !capture::enabled() {
        return;
    }
    let ev = Event::Renamed {
        moved,
        new_path: new_path.to_path_buf(),
        seq: capture::next_mark_seq(),
        covered,
    };
    events().lock().unwrap().push(ev);
    HAS_EVENTS.store(true, Ordering::Release);
}

/// REMOVE hook (fileops, post-success, regular files only).
pub fn note_remove(ident: (u64, u64)) {
    if !capture::enabled() {
        return;
    }
    events().lock().unwrap().push(Event::Removed { ident });
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
            Event::Renamed { moved, new_path, seq, covered } => {
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
            Event::Removed { ident } => backend.tier_apply_remove(*ident, ts).await,
        };
        match res {
            Ok(()) => {
                // The dead identity's RAM traces go with the applied
                // event, never before (the tx could fail).
                match ev {
                    Event::Renamed { covered: Some(c), .. } => capture::forget(c.0, c.1),
                    Event::Removed { ident } => capture::forget(ident.0, ident.1),
                    _ => {}
                }
            }
            Err(e) => {
                let mut q = events().lock().unwrap();
                let mut rest: Vec<Event> = taken[i..].to_vec();
                rest.append(&mut q);
                *q = rest;
                HAS_EVENTS.store(true, Ordering::Release);
                return Err(e);
            }
        }
    }
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
        capture::force_enable();
        let be: Arc<dyn StateBackend> = Arc::new(MemoryBackend::new());
        let (dev, moved_ino, covered_ino) = (0x1D7_u64, 0xA1_u64, 0xA2_u64);
        be.tier_upsert_generation(&gen_row(dev, covered_ino, "t/final")).await.unwrap();

        for _ in 0..50 {
            note_rename(
                Some((dev, moved_ino)),
                Path::new("/exp/final"),
                Some((dev, covered_ino)),
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
        capture::force_enable();
        let be: Arc<dyn StateBackend> = Arc::new(MemoryBackend::new());
        let (dev, ino) = (0x1D7_u64, 0xB1_u64);

        for _ in 0..50 {
            be.tier_upsert_generation(&gen_row(dev, ino, "t/victim")).await.unwrap();
            capture::note(dev, ino, capture::Mutation::Write { offset: 0, len: 4 });
            note_remove((dev, ino));
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
