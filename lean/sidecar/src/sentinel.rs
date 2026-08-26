//! The boundary verbs: `.flint/publish` and `.flint/sync` (plan §2.1,
//! §2.2 — D1, D2, D3, D3.1, D12).
//!
//! The agent touches a file; the sidecar consumes it, honors it with a
//! real barrier (or a real sync), and answers with an ack. The
//! discipline is `consume_inbox`'s, mirrored — exactly-once via
//! integrate → persist → drop, idempotent between — with rename in
//! place of CAS:
//!
//! 1. **Consume** — the sentinel is renamed out of the agent's reach
//!    into the state dir. Type-checked first (`S_ISREG` only: a FIFO
//!    would block the body read forever), body bounded at 64 KiB.
//!    **Settle-before-consume:** the poll never consumes while a
//!    pending record stands — a surviving pending must be honored,
//!    acked and retired first, or the consume would clobber it and
//!    orphan its nonces forever. Coalescing happens INSIDE the pending
//!    record: touches arriving during the min-interval wait append
//!    their nonces to the standing record.
//! 2. **Honor** — renew the lease (D12), verify we are not deposed,
//!    then run one full fused barrier (or one sync).
//! 3. **Ack** — written atomically AFTER the barrier's manifest CAS and
//!    baseline rewrite, carrying the FULL covered-nonce set: under
//!    coalescing an agent whose nonce rode behind a later touch would
//!    otherwise never see it and would re-touch in a loop, feeding the
//!    storm the rate limit exists to prevent.
//! 4. **Retire** — the pending record is removed after the ack rename.
//!
//! **The uniform crash rule (D2, replacing the draft's per-crash-point
//! matrix).** Pending-present-and-no-matching-ack is the SAME
//! observable state for "crashed before the manifest CAS" and "crashed
//! after step 7" — the baseline is rewritten only at step 7 — so acking
//! from persisted state would assert publication of writes that never
//! uploaded. On restart: pending + no matching ack ⇒ ALWAYS run a full
//! barrier (idempotent; AdoptOwn/recent-uuids covers a half-published
//! set), then ack with THAT barrier's installed seq. Pending + matching
//! ack ⇒ retire only; the ack already names a real install.
//!
//! **Refused acks (D2).** Deposal must never strand a waiting agent: on
//! `Fenced` during honor the sidecar writes `status: "refused-fenced"`
//! naming the observed epoch BEFORE the fenced exit, and flips
//! `capabilities.json` to `state: "fenced"` with no verbs.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::control::{self, write_atomic};
use super::state::ConflictRecord;
use super::{now_unix, LeanError, LeanResult, Sidecar};

/// Bound on a sentinel body read. A larger file is truncated and the
/// remainder ignored — never a wedge.
const MAX_BODY: u64 = 64 * 1024;

/// Bound on the covered-nonce set carried in one ack (oldest dropped).
const MAX_NONCES: usize = 32;

/// Bound on a scoped sync's entry list (§2.2 write containment).
pub const MAX_SCOPE_ENTRIES: usize = 64;
pub const MAX_SCOPE_ENTRY_LEN: usize = 1024;

const BUDGET: &str = "sentinel-budget.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    Publish,
    Sync,
}

impl Verb {
    pub fn as_str(&self) -> &'static str {
        match self {
            Verb::Publish => "publish",
            Verb::Sync => "sync",
        }
    }
    fn sentinel_name(&self) -> &'static str {
        match self {
            Verb::Publish => control::PUBLISH,
            Verb::Sync => control::SYNC,
        }
    }
    fn ack_name(&self) -> &'static str {
        match self {
            Verb::Publish => control::PUBLISH_ACK,
            Verb::Sync => control::SYNC_ACK,
        }
    }
    fn pending_name(&self) -> &'static str {
        match self {
            Verb::Publish => "publish.pending.json",
            Verb::Sync => "sync.pending.json",
        }
    }
    /// The consume staging name. A crash between the rename (which
    /// removes the sentinel) and the pending write would otherwise lose
    /// the touch: on restart the staging file is recovered into the
    /// pending record instead.
    fn staging_name(&self) -> &'static str {
        match self {
            Verb::Publish => "publish.consumed",
            Verb::Sync => "sync.consumed",
        }
    }
}

/// The agent's optional JSON body.
#[derive(Debug, Clone, Default, Deserialize)]
struct SentinelBody {
    #[serde(default)]
    nonce: Option<String>,
    #[serde(default)]
    note: Option<String>,
    /// `sync` only: path prefixes and exact paths (D4).
    #[serde(default)]
    scope: Option<Vec<String>>,
}

/// A consumed sentinel awaiting honor (`.flint-sync/<verb>.pending.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingSentinel {
    pub verb: String,
    /// The latest covered touch, in nanoseconds — bare-touch agents
    /// match their boundary on this.
    pub consumed_mtime_unix_ns: u128,
    pub consumed_at: u64,
    /// EVERY coalesced nonce, oldest dropped past `MAX_NONCES`.
    pub nonces: Vec<String>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub scope: Option<Vec<String>>,
    /// An unparsable or oversize body was honored as a bare touch — a
    /// warning conflict record names it. Never a wedge, never a silent
    /// drop.
    #[serde(default)]
    pub torn: bool,
}

/// The ack document (`.flint/<verb>.ack`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ack {
    /// "ok" | "refused-fenced".
    pub status: String,
    pub nonces: Vec<String>,
    pub sentinel_mtime_unix_ns: u128,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest_etag: Option<String>,
    /// "sentinel" | "sentinel-deferred" | "drain" | "recovered".
    pub boundary: String,
    pub completed_unix: u64,
    /// Set on refused-fenced: the epoch that fenced us.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_epoch: Option<u64>,
    pub report: AckReport,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AckReport {
    pub uploaded: usize,
    pub deleted: usize,
    pub parked: usize,
    pub consumed: usize,
    pub no_change: bool,
    /// `sync` only: the applied/deleted/conflict transport (§2.2 — the
    /// conflict report rides the ack in FULL: "never a silent winner"
    /// must survive the file transport).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub applied: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<ConflictRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<Vec<String>>,
    /// Foreign changes seen but deferred to the inbox flow (D4).
    #[serde(default)]
    pub out_of_scope_foreign: usize,
}

/// The work meter (D3.1 — the hot-loops no-regression rule).
///
/// A counted budget charges a hot 2 GiB checkpoint the same one unit as
/// a 4 KiB file, so a storming agent could drive `dirty_bytes × 60/hour`
/// of extra upload while staying inside a green budget — a regression on
/// the one performance sub-axis lean currently maxes (a tight local
/// loop's republish is coalesced by cadence; sentinels un-coalesce it on
/// demand). Metering work instead bounds sentinel-driven published bytes
/// at `budget × whole_put_max` per hour independent of dirty-set size,
/// and a workspace at the cap degrades to exactly today's cadence
/// behavior — the definition of no regression.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SentinelBudget {
    /// (unix, units) charges inside the rolling hour.
    pub charges: Vec<(u64, u64)>,
    /// The last sentinel-honoring barrier, for the min-interval.
    pub last_honor_unix: u64,
}

impl SentinelBudget {
    fn prune(&mut self, now: u64) {
        self.charges.retain(|(at, _)| now.saturating_sub(*at) < 3600);
    }
    pub fn spent(&self, now: u64) -> u64 {
        self.charges
            .iter()
            .filter(|(at, _)| now.saturating_sub(*at) < 3600)
            .map(|(_, u)| *u)
            .sum()
    }
    pub fn remaining(&self, now: u64, budget: u64) -> u64 {
        budget.saturating_sub(self.spent(now))
    }
}

/// Why a standing pending sentinel is not being honored right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Due {
    /// Honor it on this tick.
    Ready,
    /// Inside `sentinel_min_interval_secs` of the last honor: wait, and
    /// let further touches coalesce.
    MinInterval,
    /// The hourly budget is exhausted: the boundary is still honored,
    /// by the next FLOOR tick, and its ack is stamped
    /// `sentinel-deferred`. Contents are never thinned (D1's corollary)
    /// — cost is bounded by deferring the barrier, never by publishing
    /// less than the boundary covers.
    BudgetDeferred,
}

fn mtime_ns(m: &std::fs::Metadata) -> u128 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

fn read_bounded(path: &Path) -> std::io::Result<(Vec<u8>, bool)> {
    use std::io::Read;
    let f = std::fs::File::open(path)?;
    let mut buf = Vec::new();
    let mut take = f.take(MAX_BODY + 1);
    take.read_to_end(&mut buf)?;
    let oversize = buf.len() as u64 > MAX_BODY;
    buf.truncate(MAX_BODY as usize);
    Ok((buf, oversize))
}

impl Sidecar {
    fn pending_path(&self, verb: Verb) -> PathBuf {
        self.cfg.state_dir().join(verb.pending_name())
    }
    fn staging_path(&self, verb: Verb) -> PathBuf {
        self.cfg.state_dir().join(verb.staging_name())
    }
    fn budget_path(&self) -> PathBuf {
        self.cfg.state_dir().join(BUDGET)
    }

    pub fn load_pending(&self, verb: Verb) -> LeanResult<Option<PendingSentinel>> {
        let p = self.pending_path(verb);
        if !p.exists() {
            return Ok(None);
        }
        let bytes = std::fs::read(&p)?;
        match serde_json::from_slice(&bytes) {
            Ok(v) => Ok(Some(v)),
            // Our own record is torn (a crash mid-rewrite): honor it as
            // a bare touch rather than wedging on it.
            Err(_) => Ok(Some(PendingSentinel {
                verb: verb.as_str().into(),
                consumed_mtime_unix_ns: 0,
                consumed_at: now_unix(),
                nonces: vec![],
                note: None,
                scope: None,
                torn: true,
            })),
        }
    }

    fn save_pending(&self, verb: Verb, p: &PendingSentinel) -> LeanResult<()> {
        let bytes = serde_json::to_vec_pretty(p)
            .map_err(|e| LeanError::State(format!("pending: {e}")))?;
        write_atomic(&self.pending_path(verb), &bytes)
    }

    fn retire_pending(&self, verb: Verb) -> LeanResult<()> {
        let p = self.pending_path(verb);
        if p.exists() {
            std::fs::remove_file(&p)?;
        }
        Ok(())
    }

    pub fn load_budget(&self) -> LeanResult<SentinelBudget> {
        let p = self.budget_path();
        if !p.exists() {
            return Ok(SentinelBudget::default());
        }
        let bytes = std::fs::read(&p)?;
        Ok(serde_json::from_slice(&bytes).unwrap_or_default())
    }

    fn save_budget(&self, b: &SentinelBudget) -> LeanResult<()> {
        let bytes =
            serde_json::to_vec_pretty(b).map_err(|e| LeanError::State(format!("budget: {e}")))?;
        write_atomic(&self.budget_path(), &bytes)
    }

    /// D3.1: charge the meter for a completed sentinel honor.
    /// `published_bytes == 0` (a no-diff honor) costs NOTHING — the
    /// budget exists to bound work and a no-diff honor does none; the
    /// min-interval remains its only bound.
    pub fn charge_budget(&self, published_bytes: u64) -> LeanResult<u64> {
        let now = now_unix();
        let mut b = self.load_budget()?;
        b.prune(now);
        let units = if published_bytes == 0 {
            0
        } else {
            published_bytes.div_ceil(self.cfg.whole_put_max.max(1)).max(1)
        };
        if units > 0 {
            b.charges.push((now, units));
        }
        b.last_honor_unix = now;
        self.save_budget(&b)?;
        Ok(units)
    }

    /// Whether a standing pending sentinel may be honored on this tick.
    pub fn sentinel_due(&self) -> LeanResult<Due> {
        let now = now_unix();
        let b = self.load_budget()?;
        if b.last_honor_unix > 0
            && now.saturating_sub(b.last_honor_unix) < self.cfg.sentinel_min_interval_secs
        {
            return Ok(Due::MinInterval);
        }
        if b.remaining(now, self.cfg.sentinel_hourly_budget) == 0 {
            return Ok(Due::BudgetDeferred);
        }
        Ok(Due::Ready)
    }

    /// One poll tick: `lstat` exactly two fixed paths — no inotify
    /// dependency, no directory scan, ~2 lstats/s when idle.
    ///
    /// Returns the verbs whose sentinel was consumed (or coalesced) on
    /// this tick.
    pub fn poll_sentinels(&mut self) -> LeanResult<Vec<Verb>> {
        let mut consumed = vec![];
        for verb in [Verb::Publish, Verb::Sync] {
            if self.consume_sentinel(verb)? {
                consumed.push(verb);
            }
        }
        Ok(consumed)
    }

    /// Recover a consume that crashed between the rename and the
    /// pending write. Called at startup, before the first poll.
    pub fn recover_consume_staging(&mut self) -> LeanResult<()> {
        for verb in [Verb::Publish, Verb::Sync] {
            let staging = self.staging_path(verb);
            if staging.exists() {
                let meta = std::fs::metadata(&staging)?;
                let (body, oversize) = read_bounded(&staging)?;
                self.fold_into_pending(verb, mtime_ns(&meta), &body, oversize)?;
                std::fs::remove_file(&staging)?;
            }
        }
        Ok(())
    }

    fn consume_sentinel(&mut self, verb: Verb) -> LeanResult<bool> {
        let path = self.control_path(verb.sentinel_name());
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => return Ok(false),
        };
        // Type check FIRST: a FIFO would block the body read forever;
        // a directory, socket or symlink at the sentinel path is not a
        // touch this sidecar will act on.
        if !meta.is_file() {
            self.state.append_conflict(&ConflictRecord {
                path: format!("{}/{}", super::CONTROL_DIR, verb.sentinel_name()),
                foreign_etag: String::new(),
                preserved_key: None,
                kind: "sentinel-not-regular-file".into(),
                at_unix: now_unix(),
            })?;
            return Ok(false);
        }
        let ns = mtime_ns(&meta);
        let (body, oversize) = match read_bounded(&path) {
            Ok(v) => v,
            Err(_) => (Vec::new(), false),
        };
        // The consume act: rename the sentinel out of the agent's reach.
        // Staging first, pending second — a crash between is recovered
        // at startup from the staging file, so a touch is never lost.
        let staging = self.staging_path(verb);
        std::fs::rename(&path, &staging)?;
        self.fold_into_pending(verb, ns, &body, oversize)?;
        let _ = std::fs::remove_file(&staging);
        Ok(true)
    }

    /// Build or coalesce into the standing pending record.
    ///
    /// Settle-before-consume in its positive form: an existing pending
    /// record is never overwritten. Its nonce set grows, its covered
    /// mtime advances to the latest touch, and its scope (sync) unions —
    /// so the honor that eventually runs covers every coalesced touch,
    /// and every one of their nonces appears in the ack.
    fn fold_into_pending(
        &mut self,
        verb: Verb,
        ns: u128,
        body: &[u8],
        oversize: bool,
    ) -> LeanResult<()> {
        let trimmed = body.iter().all(|b| b.is_ascii_whitespace());
        let (parsed, torn) = if trimmed {
            (SentinelBody::default(), false) // bare touch
        } else if oversize {
            (SentinelBody::default(), true)
        } else {
            match serde_json::from_slice::<SentinelBody>(body) {
                Ok(v) => (v, false),
                Err(_) => (SentinelBody::default(), true),
            }
        };
        if torn {
            self.state.append_conflict(&ConflictRecord {
                path: format!("{}/{}", super::CONTROL_DIR, verb.sentinel_name()),
                foreign_etag: String::new(),
                preserved_key: None,
                kind: "sentinel-torn-body".into(),
                at_unix: now_unix(),
            })?;
        }
        let mut pending = self.load_pending(verb)?.unwrap_or(PendingSentinel {
            verb: verb.as_str().into(),
            consumed_mtime_unix_ns: 0,
            consumed_at: now_unix(),
            nonces: vec![],
            note: None,
            scope: None,
            torn: false,
        });
        pending.consumed_mtime_unix_ns = pending.consumed_mtime_unix_ns.max(ns);
        pending.consumed_at = now_unix();
        pending.torn |= torn;
        if let Some(n) = parsed.nonce {
            let n: String = n.chars().take(128).collect();
            if !pending.nonces.contains(&n) {
                pending.nonces.push(n);
            }
            let excess = pending.nonces.len().saturating_sub(MAX_NONCES);
            if excess > 0 {
                pending.nonces.drain(..excess);
            }
        }
        if let Some(note) = parsed.note {
            pending.note = Some(note.chars().take(4096).collect());
        }
        if let Some(scope) = parsed.scope {
            let mut merged = pending.scope.take().unwrap_or_default();
            for e in scope.into_iter().take(MAX_SCOPE_ENTRIES) {
                if e.len() > MAX_SCOPE_ENTRY_LEN {
                    continue;
                }
                if !merged.contains(&e) {
                    merged.push(e);
                }
            }
            merged.truncate(MAX_SCOPE_ENTRIES);
            pending.scope = Some(merged);
        }
        self.save_pending(verb, &pending)?;
        Ok(())
    }

    fn ack_path(&self, verb: Verb) -> PathBuf {
        self.control_path(verb.ack_name())
    }

    pub fn read_ack(&self, verb: Verb) -> Option<Ack> {
        let bytes = std::fs::read(self.ack_path(verb)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    fn write_ack(&self, verb: Verb, ack: &Ack) -> LeanResult<()> {
        let bytes =
            serde_json::to_vec_pretty(ack).map_err(|e| LeanError::State(format!("ack: {e}")))?;
        write_atomic(&self.ack_path(verb), &bytes)
    }

    /// Does a standing ack already answer this pending record? Used by
    /// the restart rule: matching ⇒ retire only (the ack names a real
    /// install); not matching ⇒ run a full barrier and ack from THAT.
    fn ack_matches(&self, verb: Verb, pending: &PendingSentinel) -> bool {
        let Some(ack) = self.read_ack(verb) else { return false };
        if ack.sentinel_mtime_unix_ns < pending.consumed_mtime_unix_ns {
            return false;
        }
        pending.nonces.iter().all(|n| ack.nonces.contains(n))
    }

    /// The refused ack (D2). Deposal must never strand a waiting agent.
    pub fn refuse_pending(&mut self, verb: Verb, observed_epoch: Option<u64>) -> LeanResult<()> {
        let Some(pending) = self.load_pending(verb)? else { return Ok(()) };
        let ack = Ack {
            status: "refused-fenced".into(),
            nonces: pending.nonces.clone(),
            sentinel_mtime_unix_ns: pending.consumed_mtime_unix_ns,
            seq: None,
            manifest_etag: None,
            boundary: "sentinel".into(),
            completed_unix: now_unix(),
            observed_epoch,
            report: AckReport::default(),
        };
        self.write_ack(verb, &ack)?;
        self.retire_pending(verb)?;
        Ok(())
    }

    /// Flip the capability marker to fenced: no verbs, `state:
    /// "fenced"`, so agents stop touching sentinels on a zombie.
    pub fn mark_fenced(&self) -> LeanResult<()> {
        let posture = self
            .load_posture()?
            .unwrap_or(super::control::SentinelPosture { enabled: false, reason: None });
        self.write_capabilities(&posture, true)
    }

    /// Honor a standing pending sentinel, if one stands and is due.
    ///
    /// `forced` = this is a floor tick picking up a budget-deferred
    /// boundary; the ack is stamped `sentinel-deferred`. `Ok(None)`
    /// means nothing was owed (or it is not due yet).
    pub async fn honor_pending(&mut self, verb: Verb, forced: bool) -> LeanResult<Option<Ack>> {
        let Some(pending) = self.load_pending(verb)? else { return Ok(None) };
        if !forced {
            match self.sentinel_due()? {
                Due::Ready => {}
                // Not honored on this tick; the floor tick will (D3).
                Due::MinInterval | Due::BudgetDeferred => return Ok(None),
            }
        }
        // The restart rule: a matching ack already names a real
        // install — retire, never re-run.
        if self.ack_matches(verb, &pending) {
            self.retire_pending(verb)?;
            return Ok(None);
        }

        // D12: every barrier-triggering arm renews first.
        if let Err(e) = super::lease::renew(self).await {
            if let LeanError::Fenced(_) = e {
                let epoch = self.observed_foreign_epoch().await;
                self.refuse_pending(verb, epoch)?;
                self.mark_fenced()?;
            }
            return Err(e);
        }

        let ack = match verb {
            Verb::Publish => self.honor_publish(&pending, forced).await,
            Verb::Sync => self.honor_sync(&pending, forced).await,
        };
        match ack {
            Ok(ack) => {
                self.write_ack(verb, &ack)?;
                self.retire_pending(verb)?;
                Ok(Some(ack))
            }
            Err(LeanError::Fenced(m)) => {
                let epoch = self.observed_foreign_epoch().await;
                self.refuse_pending(verb, epoch)?;
                self.mark_fenced()?;
                Err(LeanError::Fenced(m))
            }
            Err(e) => Err(e),
        }
    }

    /// Best-effort read of the epoch that fenced us (for the refused
    /// ack's `observed_epoch`). Never fails the refusal.
    async fn observed_foreign_epoch(&self) -> Option<u64> {
        self.store.epoch_read(&self.cfg.epoch_key()).await.ok().flatten().map(|s| s.epoch)
    }

    async fn honor_publish(&mut self, pending: &PendingSentinel, forced: bool) -> LeanResult<Ack> {
        let report = self.run_barrier().await?;
        let units = self.charge_budget(report.published_bytes)?;
        let _ = units;
        let baseline = self.state.load_baseline()?;
        Ok(Ack {
            status: "ok".into(),
            nonces: pending.nonces.clone(),
            sentinel_mtime_unix_ns: pending.consumed_mtime_unix_ns,
            seq: report.seq,
            manifest_etag: baseline.manifest_etag.clone(),
            boundary: if forced { "sentinel-deferred".into() } else { "sentinel".into() },
            completed_unix: now_unix(),
            observed_epoch: None,
            report: AckReport {
                uploaded: report.uploaded.len(),
                deleted: report.deleted.len(),
                parked: report.parked.len(),
                consumed: report.consumed,
                no_change: report.no_change,
                ..Default::default()
            },
        })
    }

    async fn honor_sync(&mut self, pending: &PendingSentinel, forced: bool) -> LeanResult<Ack> {
        // D2: `Sidecar::sync` has NO lease/epoch check of its own — it
        // is only ever reachable via `claim_then` today. A straggler
        // consuming a sync sentinel between deposal and its next
        // cooperative fence would apply the successor's manifest onto
        // its zombie tree and ack SUCCESS. The in-loop honor path
        // verifies first.
        self.verify_not_deposed_pub().await?;
        let before = self.state.load_conflicts()?.len();
        let report = self.sync_scoped(pending.scope.clone()).await?;
        let conflicts: Vec<ConflictRecord> =
            self.state.load_conflicts()?.into_iter().skip(before).collect();
        // A sync publishes no bytes: it costs no budget units, only the
        // min-interval (which it shares with publish).
        self.charge_budget(0)?;
        Ok(Ack {
            status: "ok".into(),
            nonces: pending.nonces.clone(),
            sentinel_mtime_unix_ns: pending.consumed_mtime_unix_ns,
            seq: Some(report.seq),
            manifest_etag: None,
            boundary: if forced { "sentinel-deferred".into() } else { "sentinel".into() },
            completed_unix: now_unix(),
            observed_epoch: None,
            report: AckReport {
                consumed: report.applied.len(),
                deleted: report.deleted.len(),
                applied: report.applied.clone(),
                conflicts,
                scope: pending.scope.clone(),
                out_of_scope_foreign: report.out_of_scope_foreign,
                ..Default::default()
            },
        })
    }

    /// The startup settle (the uniform crash rule). Runs before the
    /// first poll arms: a surviving pending must be honored, acked and
    /// retired before any fresh sentinel may be consumed.
    pub async fn settle_pending_at_startup(&mut self) -> LeanResult<()> {
        self.recover_consume_staging()?;
        for verb in [Verb::Publish, Verb::Sync] {
            let Some(pending) = self.load_pending(verb)? else { continue };
            if self.ack_matches(verb, &pending) {
                // Crashed after the ack, before the retire.
                self.retire_pending(verb)?;
                continue;
            }
            // Pending + no matching ack is the same observable state
            // for crash-before-CAS and crash-after-step-7: run a full
            // barrier and ack from ITS install, never from persisted
            // state.
            self.honor_pending(verb, true).await?;
        }
        Ok(())
    }
}

/// What a floor tick did.
#[derive(Debug, Default)]
pub struct FloorOutcome {
    pub seq: Option<u64>,
    pub no_change: bool,
    /// Acks written because a budget-deferred (or min-interval-held)
    /// pending sentinel was picked up by this floor tick.
    pub acks: Vec<Ack>,
    pub uploaded: usize,
    pub deleted: usize,
    pub consumed: usize,
}

impl Sidecar {
    /// Move the news ticker from what the barrier already learned (D5).
    /// Never issues a request of its own.
    fn ticker_from(&self, seq: Option<u64>, etag: Option<String>) -> LeanResult<()> {
        let integrated = self.state.load_baseline()?.seq;
        self.touch_remote_seq(seq, etag, integrated)
    }

    /// Handle a fence uniformly: settle every owed ack with a refusal,
    /// flip the capability marker, and let the caller exit. Deposal
    /// must never strand a waiting agent with a live-looking marker.
    async fn settle_fence(&mut self) -> LeanResult<()> {
        let epoch = self.observed_foreign_epoch().await;
        for verb in [Verb::Publish, Verb::Sync] {
            if self.load_pending(verb)?.is_some() {
                self.refuse_pending(verb, epoch)?;
            }
        }
        self.mark_fenced()
    }

    /// One poll tick of the sentinel arm: consume what is there, then
    /// honor if the min-interval and the work budget allow.
    ///
    /// In `cadence` mode the arm still consumes and honors — a sentinel
    /// triggers a fused barrier there too; the mode's meaning is that
    /// there is no separate citation lane, not that the verbs are dead.
    pub async fn sentinel_tick(&mut self) -> LeanResult<Vec<Ack>> {
        let posture = self.load_posture()?;
        if posture.as_ref().map(|p| !p.enabled).unwrap_or(false) {
            return Ok(vec![]);
        }
        self.poll_sentinels()?;
        let mut acks = vec![];
        // Sync before publish: a coalesced pair means "pull, then
        // publish my coherent point" — the other order would publish
        // against a tree the agent expected to have refreshed.
        for verb in [Verb::Sync, Verb::Publish] {
            match self.honor_pending(verb, false).await {
                Ok(Some(a)) => acks.push(a),
                Ok(None) => {}
                Err(e @ LeanError::Fenced(_)) => {
                    self.settle_fence().await?;
                    return Err(e);
                }
                Err(e) => return Err(e),
            }
        }
        if !acks.is_empty() {
            let last = acks.last().and_then(|a| a.seq);
            self.ticker_from(last, None)?;
        }
        Ok(acks)
    }

    /// One floor tick: renew (D12), then either honor a standing
    /// pending sentinel that the budget or min-interval held back — the
    /// boundary is honored by a REAL barrier, its ack stamped
    /// `sentinel-deferred` — or run the ordinary cadence barrier.
    ///
    /// This is the arm B16 regresses on: the interval rewrite must not
    /// let the poll arm starve cadence.
    pub async fn floor_tick(&mut self) -> LeanResult<FloorOutcome> {
        let mut out = FloorOutcome::default();
        if let Err(e) = super::lease::renew(self).await {
            if matches!(e, LeanError::Fenced(_)) {
                self.settle_fence().await?;
            }
            return Err(e);
        }
        // A held-back sync pending is settled first, for the same
        // reason as in the poll arm.
        //
        // Only a PUBLISH honor discharges the floor's own barrier: a
        // sync ack carries a seq (the manifest it synced against) but
        // publishes nothing, so treating it as "the floor ran" would
        // skip cadence for a tick — a silent RPO regression.
        let mut published = false;
        for verb in [Verb::Sync, Verb::Publish] {
            if self.load_pending(verb)?.is_some() {
                match self.honor_pending(verb, true).await {
                    Ok(Some(a)) => {
                        out.seq = a.seq.or(out.seq);
                        if verb == Verb::Publish && a.status == "ok" {
                            published = true;
                            out.uploaded = a.report.uploaded;
                            out.deleted = a.report.deleted;
                            out.consumed = a.report.consumed;
                            out.no_change = a.report.no_change;
                        }
                        out.acks.push(a);
                    }
                    Ok(None) => {}
                    Err(e @ LeanError::Fenced(_)) => {
                        self.settle_fence().await?;
                        return Err(e);
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        // A publish sentinel honored on this tick already ran the fused
        // barrier the floor owed; running a second one would be pure
        // churn.
        if !published {
            match self.run_barrier().await {
                Ok(r) => {
                    out.seq = r.seq;
                    out.no_change = r.no_change;
                    out.uploaded = r.uploaded.len();
                    out.deleted = r.deleted.len();
                    out.consumed = r.consumed;
                    self.ticker_from(r.observed_seq, r.observed_etag.clone())?;
                    return Ok(out);
                }
                Err(e @ LeanError::Fenced(_)) => {
                    self.settle_fence().await?;
                    return Err(e);
                }
                Err(e) => return Err(e),
            }
        }
        self.ticker_from(out.seq, None)?;
        Ok(out)
    }

    /// The preStop drain (D10 rule 1): settle every owed ack BEFORE the
    /// lease release. A pending sentinel at SIGTERM is answered by the
    /// drain itself — the container-restart case, where the emptyDir and
    /// the agent both survive, must not strand a waiting agent.
    pub async fn drain(&mut self) -> LeanResult<Vec<Ack>> {
        let mut acks = vec![];
        for verb in [Verb::Sync, Verb::Publish] {
            if self.load_pending(verb)?.is_some() {
                match self.honor_pending(verb, true).await {
                    Ok(Some(mut a)) => {
                        a.boundary = "drain".into();
                        // Re-write with the drain stamp so the agent can
                        // tell a drained boundary from a live one.
                        let bytes = serde_json::to_vec_pretty(&a)
                            .map_err(|e| LeanError::State(format!("ack: {e}")))?;
                        write_atomic(&self.ack_path(verb), &bytes)?;
                        acks.push(a);
                    }
                    Ok(None) => {}
                    Err(e @ LeanError::Fenced(_)) => {
                        self.settle_fence().await?;
                        return Err(e);
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        if !acks.iter().any(|a| a.seq.is_some()) {
            let r = self.run_barrier().await?;
            self.ticker_from(r.observed_seq, r.observed_etag.clone())?;
        }
        Ok(acks)
    }
}
