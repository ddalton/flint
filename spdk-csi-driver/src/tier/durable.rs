//! The durable half of dirty capture — L2 step 2 (design review A3).
//!
//! `capture` queues marks at the sync note sites; this module moves
//! them into the state backend and back out at startup. The contract
//! (A3, confirmed finding C1): a mutating op's reply must not exist
//! until the file's dirty bit is durable — the dispatcher calls
//! [`drain_pending`] after every successful content-mutating op and
//! doctors the reply to an error if the write fails. After a crash,
//! [`restore_from_backend`] marks exactly the bit-set files
//! whole-dirty in memory: the "pessimal upload, never wrong data"
//! fallback, made real.

use crate::state_backend::{StateBackend, StateBackendResult, TierDirtyEntry};
use crate::tier::capture;
use std::sync::Arc;
use tracing::{info, warn};

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Push every queued mark into the backend, one transaction. On error
/// the marks are requeued (nothing is silently dropped) and the error
/// surfaces so the dispatcher can refuse the ack.
pub async fn drain_pending(backend: &Arc<dyn StateBackend>) -> StateBackendResult<()> {
    let marks = capture::take_pending();
    if marks.is_empty() {
        return Ok(());
    }
    let ts = now_unix();
    let entries: Vec<TierDirtyEntry> = marks
        .iter()
        .map(|m| TierDirtyEntry {
            dev: m.dev,
            ino: m.ino,
            path: m.path.as_ref().map(|p| p.to_string_lossy().into_owned()),
            dirtied_unix: ts,
        })
        .collect();
    match backend.tier_mark_dirty(&entries).await {
        Ok(()) => {
            capture::confirm_durable(&marks);
            Ok(())
        }
        Err(e) => {
            capture::requeue(marks);
            Err(e)
        }
    }
}

/// Startup: every bit-set row becomes whole-dirty in memory, and its
/// bit is primed as known-durable (prime BEFORE note, or the note
/// would queue a redundant re-mark). Returns the number restored.
///
/// Rows whose path no longer resolves to their (dev, ino) are counted
/// and warned but still restored — the flusher cannot reach them by
/// path, and A7's identity-keyed rows (step 6) own that repair. The
/// skeleton's job is only to make the fallback exist.
pub async fn restore_from_backend(
    backend: &Arc<dyn StateBackend>,
) -> StateBackendResult<usize> {
    let rows = backend.tier_list_dirty().await?;
    let mut orphans = 0usize;
    for r in &rows {
        capture::prime_durable(r.dev, r.ino);
        capture::note(r.dev, r.ino, capture::Mutation::Whole);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let resolved = r.path.as_ref().and_then(|p| {
                std::fs::symlink_metadata(p).ok().map(|md| (md.dev(), md.ino()))
            });
            if resolved != Some((r.dev, r.ino)) {
                orphans += 1;
            }
        }
    }
    if orphans > 0 {
        warn!(
            "tier: {} dirty-bit row(s) no longer resolve by path — flushable again \
             once A7's identity rows land (step 6); the bit stays set and eviction \
             stays blocked for them",
            orphans
        );
    }
    if !rows.is_empty() {
        info!(
            "tier: {} file(s) restored WHOLE-DIRTY from the durable bit (A3 crash \
             fallback: pessimal upload, never wrong data)",
            rows.len()
        );
    }
    Ok(rows.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state_backend::memory::MemoryBackend;
    use crate::tier::capture::Mutation;

    /// Parallel lib tests share capture's process-global queue, so a
    /// concurrent test's dispatch can drain OUR queued mark into ITS
    /// backend between our note and our drain (production has ONE
    /// backend; only the test universe has many). This repairs a theft
    /// by re-noting and draining into OUR backend, bounded.
    async fn drain_until_row(be: &Arc<dyn StateBackend>, dev: u64, ino: u64) -> bool {
        for _ in 0..50 {
            let _ = drain_pending(be).await;
            let rows = be.tier_list_dirty().await.unwrap();
            if rows.iter().any(|r| r.dev == dev && r.ino == ino) {
                return true;
            }
            capture::clear_durable(dev, ino);
            capture::note(dev, ino, Mutation::Write { offset: 0, len: 1 });
        }
        false
    }

    #[tokio::test]
    async fn drain_writes_rows_and_confirms_durable() {
        capture::force_enable();
        let be: Arc<dyn StateBackend> = Arc::new(MemoryBackend::new());
        let (dev, ino) = (0xD8A1_u64, 0x11_u64);
        capture::note_at(
            dev,
            ino,
            Some(std::path::Path::new("/w/f.bin")),
            Mutation::Write { offset: 0, len: 10 },
        );
        assert!(drain_until_row(&be, dev, ino).await, "drain never landed the row");
        assert!(capture::is_durable(dev, ino));
        let rows = be.tier_list_dirty().await.unwrap();
        let mine = rows.iter().find(|r| r.dev == dev && r.ino == ino).unwrap();
        // Path may be None if a theft-repair rewrote it without one;
        // the first-landing row carries it in the untampered case.
        assert!(mine.path.is_none() || mine.path.as_deref() == Some("/w/f.bin"));
    }

    #[tokio::test]
    async fn restore_marks_bit_set_files_whole_dirty_and_primes() {
        capture::force_enable();
        let be: Arc<dyn StateBackend> = Arc::new(MemoryBackend::new());
        let (dev, ino) = (0xD8A1_u64, 0x22_u64);
        be.tier_mark_dirty(&[crate::state_backend::TierDirtyEntry {
            dev,
            ino,
            path: Some("/gone/file".into()),
            dirtied_unix: 1,
        }])
        .await
        .unwrap();
        let n = restore_from_backend(&be).await.unwrap();
        assert!(n >= 1);
        let c = capture::snapshot(dev, ino).expect("restored file must be captured");
        assert!(c.whole, "restore must mark WHOLE-dirty — intervals died with the process");
        assert!(
            capture::is_durable(dev, ino),
            "restore must prime the durable memo (the bit is already in the backend)"
        );
    }
}
