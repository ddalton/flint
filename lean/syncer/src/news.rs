//! The news poll (latency plan 2026-10-23, lever B).
//!
//! The bucket used to be read only inside a barrier, and a barrier runs at
//! the floor tick (`floor_secs`, 60 s) unless the agent publishes. So a
//! peer's or the UI's change reached an agent's tree in up to a minute,
//! and a gateway boundary request waited as long. While the workspace is
//! ACTIVE, this arm reads what an idle tick's first two steps read, every
//! `news_poll_secs`: a writer's cell (the gateway's requests ride it,
//! §2.5), then the manifest pointer. When the pointer names a document
//! newer than the news ticker's, the ticker moves at once (D5), and the
//! floor's barrier runs early, at most once per `news_min_interval_secs`.
//!
//! What it never does:
//! - Honour a pending publish or sync. Those stay the sentinel arm's and
//!   the floor's, under the min-interval and the hourly budget: the early
//!   barrier is `cadence_floor`, never `floor_tick`. A request it finds
//!   standing is carried, as every barrier carries one (H10).
//! - Run a barrier for this tree's own install: the ticker already names
//!   it ("our own install is the newest news", `touch_remote_seq`).
//! - Move the floor. Its interval stays independent of this arm (D3/D12):
//!   resetting it on news would let a busy peer starve the floor's
//!   deferred honours.
//! - Poll an idle workspace. Past `news_active_secs` without work the arm
//!   stops, and only the floor reads the bucket, as before.
//!
//! The model: nothing new. A poll is two reads. Folding a gateway request
//! between barriers is the sentinel arm's `Touch`, which needs only an
//! idle writer. The early barrier is a barrier, and the model does not
//! time barriers.

use std::time::{Duration, Instant};

use super::sentinel::FloorOutcome;
use super::{manifest, LeanConfig, LeanResult, Syncer};

/// The arm's schedule: whether the workspace counts as active, whether
/// news is waiting for a barrier, and when the last early barrier ran.
/// Pure: every question takes `now`.
#[derive(Debug, Clone)]
pub struct NewsClock {
    poll: Duration,
    active_for: Duration,
    min_interval: Duration,
    active_until: Instant,
    pending: bool,
    last_early: Option<Instant>,
}

impl NewsClock {
    /// A syncer starts active: an agent starts work at checkout.
    pub fn new(cfg: &LeanConfig, now: Instant) -> NewsClock {
        let active_for = Duration::from_secs(cfg.news_active_secs);
        NewsClock {
            poll: Duration::from_secs(cfg.news_poll_secs),
            active_for,
            min_interval: Duration::from_secs(cfg.news_min_interval_secs),
            active_until: now + active_for,
            pending: false,
            last_early: None,
        }
    }

    pub fn enabled(&self) -> bool {
        !self.poll.is_zero()
    }

    /// The arm's interval (a disabled arm's is never polled).
    pub fn every(&self) -> Duration {
        self.poll.max(Duration::from_secs(1))
    }

    pub fn active(&self, now: Instant) -> bool {
        self.enabled() && now < self.active_until
    }

    /// A sign of work: the active window runs from now.
    pub fn touch(&mut self, now: Instant) {
        self.active_until = self.active_until.max(now + self.active_for);
    }

    /// The pointer moved past what the tree knows.
    pub fn saw_news(&mut self, now: Instant) {
        self.pending = true;
        self.touch(now);
    }

    /// A barrier ran (any arm's, after the news): its consume read the
    /// pointer the news named, or a later one.
    pub fn taken(&mut self) {
        self.pending = false;
    }

    pub fn pending(&self) -> bool {
        self.pending
    }

    /// An early barrier started now (it counts against the interval
    /// whether or not it succeeds).
    pub fn ran_early(&mut self, now: Instant) {
        self.last_early = Some(now);
    }

    /// News is waiting and the last early barrier is at least the
    /// minimum interval ago.
    pub fn due(&self, now: Instant) -> bool {
        self.pending
            && self.last_early.map(|t| now.saturating_duration_since(t) >= self.min_interval).unwrap_or(true)
    }
}

/// Did a tick do anything a workspace counts as work?
pub fn worked(o: &FloorOutcome) -> bool {
    !o.no_change || !o.acks.is_empty() || o.uploaded > 0 || o.deleted > 0 || o.consumed > 0
}

impl Syncer {
    /// One poll: what an idle tick's first two steps read, nothing else.
    /// A writer reads the cell first, so a gateway boundary request becomes
    /// the pending publish the sentinel arm honours within its next second
    /// (§2.5's door, a second instead of a floor). Then the pointer:
    /// `Some(seq)` when it names a document newer than the ticker's
    /// `observed_seq`, which moves to it now, so the agent sees the news
    /// before a barrier takes it. `integrated_seq` stays where it was.
    pub async fn news_poll(&mut self) -> LeanResult<Option<u64>> {
        if !self.cfg.access.is_read() {
            self.read_cell_requests().await?;
        }
        // A workspace from before the pointer layout has no seq to compare:
        // its news waits for the floor.
        let Some(p) = manifest::load_pointer(self.store.as_ref(), &self.cfg).await? else {
            return Ok(None);
        };
        let t = self.load_remote_seq();
        if p.pointer.seq <= t.observed_seq {
            return Ok(None);
        }
        self.touch_remote_seq(Some(p.pointer.seq), Some(p.etag), t.integrated_seq)?;
        Ok(Some(p.pointer.seq))
    }

    /// Once, before the arm's first poll: the ticker names the document the
    /// tree holds. A checkout writes no ticker, so a fresh tree's first poll
    /// would otherwise read its own checkout as news and run a barrier.
    pub fn news_start(&self) -> LeanResult<()> {
        self.ticker_from(None, None)
    }

    /// The early barrier: the floor's own, without the floor's honours. A
    /// writer's cadence barrier, a reader's pull.
    pub async fn news_tick(&mut self) -> LeanResult<FloorOutcome> {
        let out = FloorOutcome::default();
        if self.cfg.access.is_read() {
            self.reader_floor(out).await
        } else {
            self.cadence_floor(out).await
        }
    }

    /// The loop's news arm, one tick: nothing while the workspace is idle;
    /// else a poll, and the early barrier when news is due. A failed
    /// barrier keeps the news pending and still counts against the
    /// interval, so a failing store is retried at that pace, not every
    /// second.
    pub async fn news_arm(&mut self, clock: &mut NewsClock, now: Instant) -> LeanResult<Option<FloorOutcome>> {
        if !clock.active(now) {
            return Ok(None);
        }
        if self.news_poll().await?.is_some() {
            clock.saw_news(now);
        }
        if !clock.due(now) {
            return Ok(None);
        }
        clock.ran_early(now);
        let out = self.news_tick().await?;
        clock.taken();
        Ok(Some(out))
    }
}
