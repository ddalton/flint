//! Read access (per-user access design §4.4): the floor tick of a syncer
//! that follows the workspace and never writes to the bucket.
//!
//! A writer's boundary integrates other writers' changes and the inbox
//! into its tree as a side effect of publishing. A reader has nothing to
//! publish, so its tick is the pull alone: the inbox cell and the
//! manifest pointer — the same two GETs an idle writer's tick makes —
//! and a whole-tree `sync` when either has moved since the last pull.
//! `sync` makes no store writes (`sync.rs`), applies only onto paths the
//! scan finds clean, and records every change it declines, so a reader's
//! tree converges the way a writer's does and never loses a local byte
//! silently. It never publishes one either.
//!
//! What moved is judged by the two documents' etags, remembered here
//! rather than read off the baseline: `sync` deliberately leaves
//! `baseline.manifest_etag` where it was (a writer's next merge must
//! still see a manifest change an inbox overlay hid), so the baseline
//! would report "moved" on every tick and turn an idle reader into a
//! manifest download per floor.

use serde::{Deserialize, Serialize};

use flint_store::{GenerationStamps, StoreError};

use super::sync::SyncReport;
use super::{inbox, manifest, LeanError, LeanResult, Syncer};

/// State-dir file: the documents the last completed pull synced against.
const PULLED_FROM: &str = "reader.json";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct PulledFrom {
    manifest_etag: Option<String>,
    inbox_etag: Option<String>,
}

/// What one reader tick found.
#[derive(Debug, Default)]
pub struct Pull {
    /// The manifest's seq and etag as the pointer read reported them —
    /// the news ticker's input, at no added request.
    pub observed_seq: Option<u64>,
    pub observed_etag: Option<String>,
    /// `None`: neither document moved since the last pull, and nothing
    /// was synced.
    pub sync: Option<SyncReport>,
}

impl Syncer {
    /// The refusal every publishing or fencing path gives a reader.
    /// `Refused` because it is configuration, not contention: retrying
    /// cannot make a read-only syncer write.
    pub fn refuse_if_read(&self, what: &str) -> LeanResult<()> {
        if self.cfg.access.is_read() {
            return Err(LeanError::Refused(format!(
                "{what}: this syncer has read access (FLINT_SYNC_ACCESS=read) and never writes to \
                 the bucket"
            )));
        }
        Ok(())
    }

    /// One reader tick: two GETs, and a `sync` only when there is news.
    pub async fn reader_pull(&mut self) -> LeanResult<Pull> {
        let ib = inbox::load(self.store.as_ref(), &self.cfg).await?;
        let (observed_seq, manifest_etag) =
            match manifest::load_pointer(self.store.as_ref(), &self.cfg).await? {
                Some(p) => (Some(p.pointer.seq), Some(p.etag)),
                // A workspace from before the pointer layout: the barrier's
                // own fallback, a HEAD of the legacy document.
                None => match self.store.head(&self.cfg.manifest_key()).await {
                    Ok(meta) => {
                        (GenerationStamps::from_meta(&meta.meta).map(|s| s.generation), Some(meta.etag))
                    }
                    Err(StoreError::NotFound(_)) => (None, None),
                    Err(e) => return Err(e.into()),
                },
            };
        let now = PulledFrom { manifest_etag: manifest_etag.clone(), inbox_etag: ib.etag };
        let mut out = Pull { observed_seq, observed_etag: manifest_etag, sync: None };
        if self.load_pulled_from().as_ref() == Some(&now) {
            return Ok(out);
        }
        // `sync` reads both documents again. If either moved in between,
        // what is remembered below is the OLDER etag, and the next tick
        // syncs once more — the safe direction; the other would skip news.
        out.sync = Some(self.sync().await?);
        self.save_pulled_from(&now)?;
        Ok(out)
    }

    fn load_pulled_from(&self) -> Option<PulledFrom> {
        let bytes = std::fs::read(self.cfg.state_dir().join(PULLED_FROM)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    fn save_pulled_from(&self, p: &PulledFrom) -> LeanResult<()> {
        let bytes =
            serde_json::to_vec(p).map_err(|e| LeanError::State(format!("reader memo: {e}")))?;
        // The state dir sits in the app-writable tree: the same checked
        // write every other state file takes.
        let path = self.cfg.state_dir().join(PULLED_FROM);
        super::safefs::check_parent(&path)?;
        super::safefs::write_via_tmp(&path, &path.with_extension("tmp"), &bytes, None).map(|_| ())
    }
}
