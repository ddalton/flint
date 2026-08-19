//! The RPO predicate — "can the bucket rebuild this volume right now?"
//!
//! This is the gate the lifecycle controller stands on. Suspending a
//! hub is cheap and reversible; HIBERNATING one deletes the PVC, and at
//! that moment the bucket becomes the only copy. So the question this
//! module answers is deliberately stronger than "is the dirty list
//! empty":
//!
//! - **dirty rows / pending capture marks** — bytes not yet published.
//! - **tombstones** — deletes not yet applied to the bucket, so the
//!   bucket still holds objects the tree no longer has. A restore would
//!   resurrect them.
//! - **the epoch** — a fenced or deposed hub may not publish at all,
//!   so whatever is unflushed can never be flushed by this process.
//! - **the manifest** — the subtle one. A manifest write that FAILED
//!   leaves the bucket describing an older tree, and the failure looks
//!   exactly like the common "nothing changed, skipped the write" case
//!   unless the barrier reports which happened. See
//!   [`crate::tier::manifest::BarrierOutcome`].
//! - **beyond-RPO files** — files present locally that the manifest
//!   cannot restore because they were never published.
//!
//! Every component is reported, not just the verdict: an operator
//! asking "why won't my project hibernate?" gets a specific answer.

use crate::state_backend::StateBackend;
use crate::tier::flush::FlushOrchestrator;

/// A full accounting of the volume's recovery position.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RpoStatus {
    /// The verdict: every component below is satisfied.
    pub clean: bool,
    /// Files with unpublished bytes.
    pub dirty_files: usize,
    /// Capture marks not yet pushed to the durable dirty set.
    pub pending_capture: bool,
    /// Deletes not yet applied to the bucket.
    pub tombstones: usize,
    /// Do we still hold the epoch (i.e. may we publish)?
    pub epoch_held: bool,
    /// Does the bucket's manifest describe the current tree?
    pub manifest_current: bool,
    /// The manifest generation the bucket holds, when known.
    pub manifest_seq: Option<u64>,
    /// Local files the manifest cannot restore.
    pub beyond_rpo: Option<usize>,
    /// Set when no barrier has run yet in this process — not clean, but
    /// for a benign reason that resolves on the next flush tick.
    pub awaiting_first_barrier: bool,
}

impl RpoStatus {
    /// One line explaining the verdict, for events and logs.
    pub fn why(&self) -> String {
        if self.clean {
            return "the bucket can rebuild this volume".to_string();
        }
        let mut reasons = Vec::new();
        if self.dirty_files > 0 {
            reasons.push(format!("{} unpublished file(s)", self.dirty_files));
        }
        if self.pending_capture {
            reasons.push("capture marks not yet durable".to_string());
        }
        if self.tombstones > 0 {
            reasons.push(format!("{} unapplied delete(s)", self.tombstones));
        }
        if !self.epoch_held {
            reasons.push("epoch not held (fenced or deposed)".to_string());
        }
        if self.awaiting_first_barrier {
            reasons.push("no manifest barrier has run yet".to_string());
        } else if !self.manifest_current {
            reasons.push("the manifest write failed — the bucket is behind the tree".to_string());
        }
        if let Some(n) = self.beyond_rpo.filter(|n| *n > 0) {
            reasons.push(format!("{} file(s) beyond RPO", n));
        }
        reasons.join("; ")
    }
}

/// Evaluate the predicate. Two cheap backend reads plus in-memory
/// state — no object-store round trip, so it is safe to call on every
/// status poll.
pub async fn evaluate(
    backend: &dyn StateBackend,
    epoch: &crate::tier::epoch::EpochGuard,
    orch: Option<&FlushOrchestrator>,
) -> RpoStatus {
    let dirty_files = backend.tier_list_dirty().await.map(|v| v.len()).unwrap_or(usize::MAX);
    let tombstones = backend.tier_list_tombstones().await.map(|v| v.len()).unwrap_or(usize::MAX);
    let pending_capture = crate::tier::capture::has_pending();
    let epoch_held = epoch.current().is_some();

    let barrier = orch.and_then(|o| o.last_barrier());
    let awaiting_first_barrier = barrier.is_none();
    let manifest_current = barrier.as_ref().is_some_and(|b| b.is_current());
    let manifest_seq = barrier.as_ref().and_then(|b| b.seq());
    let beyond_rpo = barrier.as_ref().and_then(|b| b.beyond_rpo());

    let clean = dirty_files == 0
        && tombstones == 0
        && !pending_capture
        && epoch_held
        && manifest_current
        && beyond_rpo == Some(0);

    RpoStatus {
        clean,
        dirty_files,
        pending_capture,
        tombstones,
        epoch_held,
        manifest_current,
        manifest_seq,
        beyond_rpo,
        awaiting_first_barrier,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state_backend::memory::MemoryBackend;
    use crate::tier::epoch::EpochGuard;
    use std::sync::Arc;

    /// A backend read failure must never read as "clean" — the
    /// saturating `usize::MAX` makes an unreachable database look like
    /// unpublished work, not like a volume safe to hibernate.
    #[tokio::test]
    async fn a_held_epoch_alone_is_not_clean() {
        let backend = MemoryBackend::new();
        let guard = Arc::new(EpochGuard::held(1));
        // No orchestrator ⇒ no barrier has run ⇒ not clean, and the
        // reason says so rather than blaming a phantom dirty file.
        let st = evaluate(&backend, &guard, None).await;
        assert!(!st.clean);
        assert!(st.awaiting_first_barrier);
        assert!(st.epoch_held);
        assert!(st.why().contains("no manifest barrier"));
    }

    /// The gate that decides whether a shutdown may mark the epoch
    /// released — and, later, whether the PVC may be deleted. A volume
    /// with a published tree and a current manifest is clean; dirty
    /// the tree and it must stop being clean immediately, because the
    /// bucket can no longer rebuild what the PVC holds.
    #[tokio::test]
    async fn a_published_tree_is_clean_until_something_is_dirtied() {
        use crate::state_backend::TierDirtyEntry;
        use crate::tier::flush::{FlushConfig, FlushOrchestrator};
        use crate::tier::manifest;
        use crate::tier::store::{memory::MemoryStore, ObjectStore};

        let dir = tempfile::TempDir::new().unwrap();
        let backend: Arc<dyn StateBackend> = Arc::new(MemoryBackend::new());
        let store: Arc<dyn ObjectStore> = Arc::new(MemoryStore::new());
        let guard = EpochGuard::held(1);
        let orch = FlushOrchestrator::new(
            store.clone(),
            backend.clone(),
            FlushConfig::new(dir.path().to_path_buf(), "t/".into()),
            guard.clone(),
        );

        // No barrier has run: NOT clean, and honest about why. This is
        // the freshly-started hub, and hibernating it on an empty dirty
        // list would delete a PVC the bucket has never described.
        let st = evaluate(backend.as_ref(), &guard, Some(&orch)).await;
        assert!(!st.clean && st.awaiting_first_barrier);

        // A barrier lands ⇒ the bucket now describes the tree.
        orch.write_manifest_barrier().await;
        let st = evaluate(backend.as_ref(), &guard, Some(&orch)).await;
        assert!(st.clean, "after a barrier over a clean tree: {}", st.why());
        assert!(st.manifest_current);

        // An unflushed write ⇒ not clean, whatever the manifest says.
        backend
            .tier_mark_dirty(&[TierDirtyEntry {
                dev: 1,
                ino: 2,
                path: Some(dir.path().join("new.bin").display().to_string()),
                dirtied_unix: 0,
                mark_seq: 1,
            }])
            .await
            .unwrap();
        let st = evaluate(backend.as_ref(), &guard, Some(&orch)).await;
        assert!(!st.clean);
        assert_eq!(st.dirty_files, 1);
        assert!(st.why().contains("unpublished"));

        // And a manifest that could not be written is NOT the same as
        // one that did not need writing — the distinction the old
        // Option<u64> return threw away.
        let unchanged = manifest::BarrierOutcome::Unchanged {
            seq: 3,
            beyond_rpo: 0,
            skipped_special: 0,
        };
        assert!(unchanged.is_current());
        assert!(!manifest::BarrierOutcome::Failed.is_current());
    }

    #[tokio::test]
    async fn a_fenced_hub_is_never_clean() {
        let backend = MemoryBackend::new();
        let guard = Arc::new(EpochGuard::held(1));
        guard.fence();
        let st = evaluate(&backend, &guard, None).await;
        assert!(!st.clean);
        assert!(!st.epoch_held);
        assert!(st.why().contains("epoch not held"));
    }
}
