//! READ-delegation state core — slice 3 of
//! docs/plans/nfs-delegations-design.md, modeled by
//! formal/FlintDelegRecall.tla (GATING — keep the two in lockstep).
//!
//! Everything here is inert until the grant path goes live behind
//! FLINT_NFS_DELEGATIONS: the managers hold no records, every fence
//! consult answers Clear, and the DELAY/recall arms are unreachable.
//!
//! Design invariants this file owns:
//! - Files are keyed by `FileId(dev, ino)`, never by fh bytes or path:
//!   in path-handles mode hardlinks alias one inode under different
//!   fhs, and `fh_kernel::try_new` falls back to path handles silently.
//! - The mutation-pending guard is RAII and registered at CONSULT time
//!   under the entry lock, held until the mutation completes. The
//!   grant's under-lock re-check refuses while any guard is live —
//!   this is the model's MutationGuard constant, and the NoGuard
//!   mutation run is the counterexample for removing it.
//! - A retained REVOKED record blocks new grants on the file until it
//!   is freed (FREE_STATEID / client teardown) — the model's revTomb.
//! - Recall is single-flight per record: later conflicts on the same
//!   file see state != Granted and answer DELAY without re-sending.

use super::super::protocol::StateId;
use dashmap::DashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// Whether the server may grant delegations (FLINT_NFS_DELEGATIONS=1,
/// read once). Every fence consult and every grant checks this first,
/// so the OFF default costs one atomic load on the mutation hot paths
/// and nothing else. Tests flip the override — the env OnceLock is
/// process-wide and useless to them.
pub fn delegations_enabled() -> bool {
    match DELEG_OVERRIDE.load(Ordering::Relaxed) {
        1 => true,
        2 => false,
        _ => {
            static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            *ENABLED.get_or_init(|| {
                std::env::var("FLINT_NFS_DELEGATIONS")
                    .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                    .unwrap_or(false)
            })
        }
    }
}

/// 0 = env, 1 = forced on, 2 = forced off. Test-only knob — but it
/// must exist in the production build because integration tests link
/// the real dispatcher.
static DELEG_OVERRIDE: AtomicU8 = AtomicU8::new(0);

/// Force the delegation gate for tests (process-wide; tests that flip
/// it on must tolerate fence consults running everywhere).
pub fn override_delegations_enabled(on: Option<bool>) {
    DELEG_OVERRIDE.store(
        match on {
            None => 0,
            Some(true) => 1,
            Some(false) => 2,
        },
        Ordering::Relaxed,
    );
}

/// File identity: `(dev, ino)`. The same key the F14 change counter
/// uses, for the same reason — fhs and paths both alias.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileId {
    pub dev: u64,
    pub ino: u64,
}

impl FileId {
    pub fn new(dev: u64, ino: u64) -> Self {
        Self { dev, ino }
    }
}

/// Per-record lifecycle. `Granted → RecallPending → RecallAcked →`
/// dropped via DELEGRETURN; any `Recall*` state may hit the revoke
/// deadline and become `Revoked` (record RETAINED as the tombstone the
/// SEQ4 bit refers to). Client teardown drops records in any state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelegState {
    Granted,
    RecallPending,
    RecallAcked,
    Revoked,
}

/// One granted delegation. `fh` is stored verbatim-as-granted and
/// echoed in CB_RECALL — byte-identical in both fh modes. `path` is
/// for operators and logs; identity decisions use `ident` only.
#[derive(Debug)]
pub struct DelegRecord {
    pub stateid: StateId,
    pub client_id: u64,
    pub fh: Vec<u8>,
    pub ident: FileId,
    pub path: PathBuf,
    pub state: DelegState,
    pub granted_at: Instant,
    /// Set when a conflict first transitioned this record out of
    /// Granted (the DELAY clock for the conflictor, not the revoke
    /// clock).
    pub recall_started_at: Option<Instant>,
    /// Set on the first SUCCESSFUL CB_RECALL transmit. The 90s revoke
    /// deadline runs from HERE, not from conflict detection — slot-0
    /// serialization to one slow client must not revoke delegations
    /// that were never asked for.
    pub first_transmit_at: Option<Instant>,
    /// CB_RECALL truncate hint (REMOVE sets it).
    pub truncate: bool,
}

/// Everything the recall path needs to send one CB_RECALL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecallOrder {
    pub stateid: StateId,
    pub client_id: u64,
    pub fh: Vec<u8>,
    pub truncate: bool,
}

struct FileEntry {
    records: Vec<DelegRecord>,
    /// Live mutation-pending guards (consult-to-completion). Grants
    /// refuse while nonzero.
    live_guards: u64,
    /// Post-recall cooldown: grants refuse until this instant, so an
    /// alternating writer can't drive grant/recall thrash with no
    /// damping below the global breaker.
    cooldown_until: Option<Instant>,
    /// Set (under the entry lock, inside the map-removal predicate)
    /// when this entry is GC'd. A thread that obtained the Arc before
    /// the removal and locks it after must retry through the map —
    /// otherwise its work lands in an orphan while a sibling creates a
    /// second lock for the same file.
    dead: bool,
}

impl FileEntry {
    fn new() -> Self {
        Self {
            records: Vec::new(),
            live_guards: 0,
            cooldown_until: None,
            dead: false,
        }
    }

    fn is_empty(&self) -> bool {
        self.records.is_empty() && self.live_guards == 0 && self.cooldown_until.is_none()
    }
}

/// The one GC: remove the entry iff empty, marking it dead under its
/// own lock INSIDE the predicate so the removal and the marking are
/// one atomic step against the shard lock.
fn gc_map_entry(files: &DashMap<FileId, Arc<Mutex<FileEntry>>>, ident: FileId) {
    files.remove_if(&ident, |_, e| {
        e.lock()
            .map(|mut g| {
                if g.is_empty() {
                    g.dead = true;
                    true
                } else {
                    false
                }
            })
            .unwrap_or(false)
    });
}

/// RAII mutation-pending guard. Registered under the entry lock at
/// fence-consult time; dropping it (mutation complete, or the op
/// answered DELAY and gave up) releases the grant block — and GCs the
/// file entry if the guard was the only thing keeping it alive, so
/// flag-on mutation traffic over undelegated files does not grow the
/// map without bound.
pub struct MutationGuard {
    entry: Arc<Mutex<FileEntry>>,
    files: Arc<DashMap<FileId, Arc<Mutex<FileEntry>>>>,
    ident: FileId,
}

impl Drop for MutationGuard {
    fn drop(&mut self) {
        {
            let mut e = self.entry.lock().unwrap();
            debug_assert!(e.live_guards > 0);
            e.live_guards = e.live_guards.saturating_sub(1);
        }
        gc_map_entry(&self.files, self.ident);
    }
}

/// Fence-consult verdict. `Clear` carries only the guard; `Conflict`
/// additionally carries the CB_RECALLs to send (only records THIS
/// consult transitioned out of Granted — single-flight) and whether
/// the mutation must be answered NFS4ERR_DELAY. `delay == false` with
/// a nonempty `recalls` is the self-conflict carve-out: the mutator is
/// the sole holder, so its own delegation is recalled but the op
/// proceeds (re-DELAYing the sole holder's O_TRUNC SETATTR one op
/// after the carve-out exempted its OPEN would nullify it).
pub enum FenceOutcome {
    Clear(MutationGuard),
    Conflict {
        guard: MutationGuard,
        recalls: Vec<RecallOrder>,
        delay: bool,
    },
}

impl FenceOutcome {
    /// The guard, whichever arm. For call sites that only need to hold
    /// the fence open across execution.
    pub fn guard(self) -> MutationGuard {
        match self {
            FenceOutcome::Clear(g) => g,
            FenceOutcome::Conflict { guard, .. } => guard,
        }
    }
}

/// What a mutation lane does with its fence consult
/// (`StateManager::deleg_fence`): proceed — holding the guard, when
/// one was minted, until the mutation completes — or answer
/// NFS4ERR_DELAY and give up this attempt.
pub enum FenceVerdict {
    Proceed(Option<MutationGuard>),
    Delay,
}

/// Why a grant was refused. Every refusal is free (OPEN_DELEGATE_NONE,
/// never DELAY) and counted per-reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantRefusal {
    /// A record in Recall*/Revoked state exists on the file.
    Barrier,
    /// A live mutation-pending guard exists on the file.
    MutationPending,
    /// Inside the post-recall cooldown window.
    Cooldown,
    /// The requester already holds a live delegation on this file.
    AlreadyHolder,
    QuotaClient,
    QuotaGlobal,
    /// The caller's under-lock re-check (write opens, write-capable
    /// layouts) said no.
    Precheck,
}

impl GrantRefusal {
    pub fn counter_name(self) -> &'static str {
        match self {
            GrantRefusal::Barrier => "barrier",
            GrantRefusal::MutationPending => "mutation_pending",
            GrantRefusal::Cooldown => "cooldown",
            GrantRefusal::AlreadyHolder => "already_holder",
            GrantRefusal::QuotaClient => "quota_client",
            GrantRefusal::QuotaGlobal => "quota_global",
            GrantRefusal::Precheck => "precheck",
        }
    }
}

/// Snapshot of one record for the ladder task and tests (records
/// themselves are not Clone — they'll grow a JoinHandle).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegSnapshot {
    pub stateid: StateId,
    pub client_id: u64,
    pub state: DelegState,
    pub truncate: bool,
}

pub struct DelegationManager {
    files: Arc<DashMap<FileId, Arc<Mutex<FileEntry>>>>,
    /// stateid.other → file, so per-stateid ops are point lookups.
    by_stateid: DashMap<[u8; 12], FileId>,
    /// client → stateid.others, EVERY state including revoked
    /// tombstones (a tombstone is still recorded state — it keeps
    /// DESTROY_CLIENTID answering CLIENTID_BUSY until freed).
    by_client: DashMap<u64, Vec<[u8; 12]>>,
    /// Live (non-revoked) records per client. Quotas bound the LIVE
    /// set; tombstones don't count (they were capped by the quota at
    /// the moment of revocation and are freed on FREE_STATEID or
    /// teardown).
    live_per_client: DashMap<u64, u64>,
    /// Live (non-revoked) records, all files.
    live_global: AtomicU64,
    max_per_client: u64,
    max_global: u64,
    cooldown: Duration,
    /// Per-reason grant refusals (metrics surface reads these).
    refusals: DashMap<&'static str, u64>,
    /// Grants ever made (metrics).
    grants_total: AtomicU64,
    /// Recent revocation stamps, pruned to the 5-minute breaker
    /// window (design §10 layer 3). Global trip stops NEW grants only
    /// — recalls, returns, and SEQ4 keep running so state drains —
    /// and resets itself once the window rolls quiet.
    revocations: Mutex<Vec<Instant>>,
    /// Same, per client: one NAT-broken fleet member must not darken
    /// delegations for everyone, so its grants are refused first.
    revocations_by_client: DashMap<u64, Vec<Instant>>,
    breaker_trip: usize,
    breaker_client_trip: usize,
    /// Sentinel kill-switch cache (design §10 layer 2): the presence
    /// of `<export>/.flint-nfs/deleg-off`, re-checked at most every
    /// ~5s. The true manual, no-restart grant stop.
    sentinel_cache: Mutex<Option<(Instant, bool)>>,
}

/// Breaker window (design §10: revocations within 5 minutes).
const BREAKER_WINDOW: Duration = Duration::from_secs(300);
/// Sentinel re-check interval.
const SENTINEL_TTL: Duration = Duration::from_secs(5);

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

impl DelegationManager {
    pub fn new() -> Self {
        Self::with_limits(
            env_u64("FLINT_NFS_DELEG_MAX_PER_CLIENT", 4096),
            env_u64("FLINT_NFS_DELEG_MAX_GLOBAL", 65536),
            Duration::from_secs(30),
        )
    }

    /// Test/tuning constructor: quotas + post-recall cooldown.
    pub fn with_limits(max_per_client: u64, max_global: u64, cooldown: Duration) -> Self {
        Self {
            files: Arc::new(DashMap::new()),
            by_stateid: DashMap::new(),
            by_client: DashMap::new(),
            live_per_client: DashMap::new(),
            live_global: AtomicU64::new(0),
            max_per_client,
            max_global,
            cooldown,
            refusals: DashMap::new(),
            grants_total: AtomicU64::new(0),
            revocations: Mutex::new(Vec::new()),
            revocations_by_client: DashMap::new(),
            breaker_trip: env_u64("FLINT_NFS_DELEG_REVOKE_TRIP", 10) as usize,
            breaker_client_trip: env_u64("FLINT_NFS_DELEG_CLIENT_REVOKE_TRIP", 3) as usize,
            sentinel_cache: Mutex::new(None),
        }
    }

    /// Is the automatic circuit breaker refusing NEW grants — either
    /// globally (fleet-wide revocation storm) or for this client (its
    /// own recalls keep dying)? Pruning happens on read, so a quiet
    /// window auto-resets the trip.
    pub fn grants_paused(&self, client_id: u64) -> bool {
        let now = Instant::now();
        {
            let mut v = self.revocations.lock().unwrap();
            v.retain(|t| now.duration_since(*t) < BREAKER_WINDOW);
            if v.len() >= self.breaker_trip {
                return true;
            }
        }
        if let Some(mut v) = self.revocations_by_client.get_mut(&client_id) {
            v.retain(|t| now.duration_since(*t) < BREAKER_WINDOW);
            if v.len() >= self.breaker_client_trip {
                return true;
            }
        }
        false
    }

    /// Is the sentinel kill-switch file present under the export?
    /// Cached ~5s; the watcher IS this cache (no background task).
    pub fn sentinel_blocked(&self, export_root: &std::path::Path) -> bool {
        let mut cache = self.sentinel_cache.lock().unwrap();
        if let Some((at, val)) = *cache {
            if at.elapsed() < SENTINEL_TTL {
                return val;
            }
        }
        let present = export_root.join(".flint-nfs").join("deleg-off").exists();
        *cache = Some((Instant::now(), present));
        present
    }

    fn entry(&self, ident: FileId) -> Arc<Mutex<FileEntry>> {
        self.files
            .entry(ident)
            .or_insert_with(|| Arc::new(Mutex::new(FileEntry::new())))
            .clone()
    }

    /// Run `f` with the file entry LOCKED, retrying if the Arc we
    /// fetched was GC'd before we got the lock (FileEntry::dead). The
    /// closure also receives the Arc so it can mint guards.
    fn with_live_entry<R>(
        &self,
        ident: FileId,
        f: impl FnOnce(&Arc<Mutex<FileEntry>>, &mut FileEntry) -> R,
    ) -> R {
        loop {
            let entry = self.entry(ident);
            let mut g = entry.lock().unwrap();
            if g.dead {
                drop(g);
                continue;
            }
            return f(&entry, &mut g);
        }
    }

    fn note_refusal(&self, r: GrantRefusal) -> GrantRefusal {
        *self.refusals.entry(r.counter_name()).or_insert(0) += 1;
        r
    }

    /// Count a handler-level refusal (gate/grace/claim/share_want/
    /// no_cb/no_ident — the rules that run before the entry lock).
    /// Same counter family as the manager's own reasons, so the
    /// grant/refusal split reads as one surface.
    pub fn count_refusal(&self, reason: &'static str) {
        *self.refusals.entry(reason).or_insert(0) += 1;
    }

    /// Read any refusal counter by name (metrics + rigs).
    pub fn refusal_count_named(&self, reason: &str) -> u64 {
        self.refusals.get(reason).map(|v| *v).unwrap_or(0)
    }

    /// Grant a READ delegation on `ident` to `client_id`, or say why
    /// not. `precheck` runs UNDER the entry lock and must re-verify
    /// the conditions the caller established outside it (no write
    /// opens, no write-capable layout) — the lock is what makes the
    /// grant atomic against the fence protocol: every interleaving
    /// either sees the mutator (refuse) or the mutator's consult sees
    /// the record (recall). `mint` allocates the stateid via
    /// `StateIdManager::allocate(StateType::Delegation, ..)` so
    /// delegation stateids live in the ONE stateid namespace
    /// (READ/TEST_STATEID/FREE_STATEID work; no disjoint-namespace
    /// BAD_STATEID trap).
    pub fn try_grant(
        &self,
        ident: FileId,
        client_id: u64,
        fh: Vec<u8>,
        path: PathBuf,
        precheck: impl FnOnce() -> bool,
        mint: impl FnOnce() -> StateId,
    ) -> Result<StateId, GrantRefusal> {
        let granted = self.with_live_entry(ident, |_entry, e| {
            if e.live_guards > 0 {
                return Err(self.note_refusal(GrantRefusal::MutationPending));
            }
            if e.records.iter().any(|r| r.state != DelegState::Granted) {
                return Err(self.note_refusal(GrantRefusal::Barrier));
            }
            if let Some(until) = e.cooldown_until {
                if Instant::now() < until {
                    return Err(self.note_refusal(GrantRefusal::Cooldown));
                }
                e.cooldown_until = None;
            }
            if e.records.iter().any(|r| r.client_id == client_id) {
                return Err(self.note_refusal(GrantRefusal::AlreadyHolder));
            }
            let client_live = self.live_per_client.get(&client_id).map(|v| *v).unwrap_or(0);
            if client_live >= self.max_per_client {
                return Err(self.note_refusal(GrantRefusal::QuotaClient));
            }
            if self.live_global.load(Ordering::SeqCst) >= self.max_global {
                return Err(self.note_refusal(GrantRefusal::QuotaGlobal));
            }
            if !precheck() {
                return Err(self.note_refusal(GrantRefusal::Precheck));
            }

            let stateid = mint();
            e.records.push(DelegRecord {
                stateid,
                client_id,
                fh,
                ident,
                path: path.clone(),
                state: DelegState::Granted,
                granted_at: Instant::now(),
                recall_started_at: None,
                first_transmit_at: None,
                truncate: false,
            });
            Ok(stateid)
        });
        let stateid = granted?;

        self.by_stateid.insert(stateid.other, ident);
        self.by_client
            .entry(client_id)
            .or_insert_with(Vec::new)
            .push(stateid.other);
        *self.live_per_client.entry(client_id).or_insert(0) += 1;
        self.live_global.fetch_add(1, Ordering::SeqCst);
        self.grants_total.fetch_add(1, Ordering::SeqCst);
        info!(
            "deleg: granted READ delegation {:?} on {:?} to client {}",
            stateid, path, client_id
        );
        Ok(stateid)
    }

    /// The ONE conflict funnel (design §5.2). Registers a mutation-
    /// pending guard under the entry lock, transitions conflicting
    /// Granted records to RecallPending (single-flight), and decides
    /// DELAY. `mutator == None` means a server-local lane (file API,
    /// REST): every holder is "another client". `truncate` is the
    /// CB_RECALL hint (REMOVE passes true).
    ///
    /// The self-conflict carve-out lives HERE and nowhere else: a
    /// mutator that is the SOLE holder gets its record recalled but
    /// `delay == false`, and because it is in the funnel, every fence
    /// site inherits it.
    pub fn mutation_fence(
        &self,
        ident: FileId,
        mutator: Option<u64>,
        truncate: bool,
    ) -> FenceOutcome {
        self.with_live_entry(ident, |entry, e| {
            e.live_guards += 1;
            let guard = MutationGuard {
                entry: Arc::clone(entry),
                files: Arc::clone(&self.files),
                ident,
            };

            if e.records.is_empty() {
                return FenceOutcome::Clear(guard);
            }

            // A revoked tombstone is not live state: it neither delays
            // the mutation nor needs recalling. (The barrier it holds
            // is against GRANTS, not mutations.)
            let mut recalls = Vec::new();
            let mut foreign_live = false;
            let now = Instant::now();
            for r in e.records.iter_mut() {
                match r.state {
                    DelegState::Revoked => continue,
                    DelegState::RecallPending | DelegState::RecallAcked => {
                        if Some(r.client_id) != mutator {
                            foreign_live = true;
                        }
                    }
                    DelegState::Granted => {
                        if Some(r.client_id) != mutator {
                            foreign_live = true;
                        }
                        r.state = DelegState::RecallPending;
                        r.recall_started_at = Some(now);
                        r.truncate = truncate;
                        recalls.push(RecallOrder {
                            stateid: r.stateid,
                            client_id: r.client_id,
                            fh: r.fh.clone(),
                            truncate,
                        });
                    }
                }
            }

            if recalls.is_empty() && !foreign_live {
                // Only tombstones on the file.
                return FenceOutcome::Clear(guard);
            }
            FenceOutcome::Conflict {
                guard,
                recalls,
                delay: foreign_live,
            }
        })
    }

    /// Live delegation lookup: (owning client, path). Conversion opens
    /// (CLAIM_DELEGATE_CUR / CLAIM_DELEG_CUR_FH) validate against this;
    /// a revoked tombstone answers None (the conversion then fails and
    /// the client learns the truth via TEST_STATEID → DELEG_REVOKED).
    pub fn lookup(&self, stateid: &StateId) -> Option<(u64, PathBuf)> {
        let ident = *self.by_stateid.get(&stateid.other)?;
        let entry = self.files.get(&ident)?.clone();
        let e = entry.lock().unwrap();
        e.records
            .iter()
            .find(|r| r.stateid.other == stateid.other && r.state != DelegState::Revoked)
            .map(|r| (r.client_id, r.path.clone()))
    }

    /// DELEGRETURN (design §5.4): delegation stateids keep seqid 1 for
    /// life. seqid 0 is the resolved "current" form; 1 matches; any
    /// other seqid ⇒ OLD_STATEID. Unknown ⇒ BAD_STATEID. Revoked ⇒
    /// DELEG_REVOKED with the tombstone retained. On OK the record is
    /// dropped and, if it was under recall, the barrier lifts and the
    /// cooldown starts.
    ///
    /// Status is returned as a name, not Nfs4Status, to keep this
    /// module protocol-agnostic; the dispatcher maps it.
    pub fn return_delegation(&self, stateid: &StateId) -> Result<u64, DelegReturnError> {
        let ident = match self.by_stateid.get(&stateid.other) {
            Some(i) => *i,
            None => return Err(DelegReturnError::Unknown),
        };
        let entry = match self.files.get(&ident) {
            Some(e) => e.clone(),
            None => return Err(DelegReturnError::Unknown),
        };
        let mut e = entry.lock().unwrap();
        let idx = match e
            .records
            .iter()
            .position(|r| r.stateid.other == stateid.other)
        {
            Some(i) => i,
            None => return Err(DelegReturnError::Unknown),
        };
        if e.records[idx].state == DelegState::Revoked {
            return Err(DelegReturnError::Revoked);
        }
        if stateid.seqid != 0 && stateid.seqid != 1 {
            return Err(DelegReturnError::OldSeqid);
        }
        let was_under_recall = e.records[idx].state != DelegState::Granted;
        let rec = e.records.remove(idx);
        if was_under_recall && !e.records.iter().any(|r| r.state != DelegState::Revoked) {
            e.cooldown_until = Some(Instant::now() + self.cooldown);
        }
        drop(e);
        self.unindex(&rec);
        info!(
            "deleg: client {} returned delegation {:?} on {:?}",
            rec.client_id, rec.stateid, rec.path
        );
        Ok(rec.client_id)
    }

    /// First successful CB_RECALL transmit for this record — starts the
    /// revoke clock. No-ops on record-gone/state-changed (a ladder
    /// wakeup must never act on a stale view).
    pub fn note_first_transmit(&self, stateid: &StateId) {
        self.with_record(stateid, |r| {
            if r.state == DelegState::RecallPending && r.first_transmit_at.is_none() {
                r.first_transmit_at = Some(Instant::now());
            }
        });
    }

    /// CB_RECALL answered NFS4_OK: the client acknowledged and will
    /// DELEGRETURN. No-ops unless currently RecallPending.
    pub fn note_recall_acked(&self, stateid: &StateId) {
        self.with_record(stateid, |r| {
            if r.state == DelegState::RecallPending {
                r.state = DelegState::RecallAcked;
            }
        });
    }

    /// Revoke deadline hit (or definitive callback refusal): mark the
    /// record Revoked and RETAIN it — the tombstone is what
    /// TEST_STATEID answers DELEG_REVOKED about and what blocks
    /// re-grants until freed. Returns the owning client (for the SEQ4
    /// raise) iff this call did the transition. Lifts the mutation
    /// barrier (the record leaves the live set) and starts cooldown.
    pub fn revoke(&self, stateid: &StateId) -> Option<u64> {
        let ident = *self.by_stateid.get(&stateid.other)?;
        let entry = self.files.get(&ident)?.clone();
        let mut e = entry.lock().unwrap();
        let r = e
            .records
            .iter_mut()
            .find(|r| r.stateid.other == stateid.other)?;
        if r.state == DelegState::Revoked || r.state == DelegState::Granted {
            // Granted records are never revoked — only records a recall
            // was actually attempted on ("revoke only from recall", the
            // model's Inv_RevokeOnlyFromRecall).
            return None;
        }
        r.state = DelegState::Revoked;
        let client = r.client_id;
        warn!(
            "deleg: REVOKED delegation {:?} on {:?} held by client {} (recall not honored)",
            r.stateid, r.path, client
        );
        if !e.records.iter().any(|x| x.state != DelegState::Revoked) {
            e.cooldown_until = Some(Instant::now() + self.cooldown);
        }
        drop(e);
        // The tombstone leaves the LIVE accounting (quotas, idle
        // input) but stays in by_client — it is still recorded state
        // until FREE_STATEID or teardown.
        self.live_global.fetch_sub(1, Ordering::SeqCst);
        self.dec_live_client(client);
        // Feed the breaker (design §10 layer 3): enough of these in
        // the window pauses new grants, per-client first.
        let now = Instant::now();
        self.revocations.lock().unwrap().push(now);
        self.revocations_by_client
            .entry(client)
            .or_insert_with(Vec::new)
            .push(now);
        Some(client)
    }

    /// The disown resolution (design §5.3): the client answered the
    /// recall with BAD_STATEID/BADHANDLE and the delayed re-probe
    /// confirmed it — it genuinely does not hold this delegation.
    /// Drop the record like a completed return: barrier lifts,
    /// cooldown starts (the conflictor's retry still beats re-grants).
    /// Only records under recall qualify; anything else no-ops (a
    /// racing DELEGRETURN or teardown already resolved it).
    pub fn resolve_disown(&self, stateid: &StateId) -> bool {
        let ident = match self.by_stateid.get(&stateid.other) {
            Some(i) => *i,
            None => return false,
        };
        let entry = match self.files.get(&ident) {
            Some(e) => e.clone(),
            None => return false,
        };
        let mut e = entry.lock().unwrap();
        let idx = e.records.iter().position(|r| {
            r.stateid.other == stateid.other
                && matches!(r.state, DelegState::RecallPending | DelegState::RecallAcked)
        });
        let idx = match idx {
            Some(i) => i,
            None => return false,
        };
        let rec = e.records.remove(idx);
        if !e.records.iter().any(|r| r.state != DelegState::Revoked) {
            e.cooldown_until = Some(Instant::now() + self.cooldown);
        }
        drop(e);
        info!(
            "deleg: client {} disowned delegation {:?} on {:?} — dropped after re-probe",
            rec.client_id, rec.stateid, rec.path
        );
        self.unindex(&rec);
        true
    }

    /// FREE_STATEID on a revoked tombstone: drop it for good. Answers
    /// whether a tombstone was actually freed. (Freeing a LIVE
    /// delegation is refused — that's DELEGRETURN's job.)
    pub fn free_revoked(&self, stateid: &StateId) -> bool {
        let ident = match self.by_stateid.get(&stateid.other) {
            Some(i) => *i,
            None => return false,
        };
        let entry = match self.files.get(&ident) {
            Some(e) => e.clone(),
            None => return false,
        };
        let mut e = entry.lock().unwrap();
        let idx = match e
            .records
            .iter()
            .position(|r| r.stateid.other == stateid.other && r.state == DelegState::Revoked)
        {
            Some(i) => i,
            None => return false,
        };
        let rec = e.records.remove(idx);
        drop(e);
        self.by_stateid.remove(&rec.stateid.other);
        if let Some(mut v) = self.by_client.get_mut(&rec.client_id) {
            v.retain(|o| o != &rec.stateid.other);
        }
        self.gc_entry(ident);
        true
    }

    /// Client teardown cascade (lease expiry, DESTROY_CLIENTID,
    /// EXCHANGE_ID replacement): drop every record — any state — and
    /// return the freed stateids so the caller can clean the
    /// StateIdManager side. Teardown of a record under recall lifts
    /// the file barrier so DELAYed conflictors proceed.
    pub fn cleanup_client_delegations(&self, client_id: u64) -> Vec<StateId> {
        let others = match self.by_client.remove(&client_id) {
            Some((_, v)) => v,
            None => return Vec::new(),
        };
        let mut freed = Vec::new();
        for other in others {
            let ident = match self.by_stateid.remove(&other) {
                Some((_, i)) => i,
                None => continue,
            };
            let entry = match self.files.get(&ident) {
                Some(e) => e.clone(),
                None => continue,
            };
            let mut e = entry.lock().unwrap();
            let idx = e.records.iter().position(|r| r.stateid.other == other);
            if let Some(idx) = idx {
                let rec = e.records.remove(idx);
                if rec.state != DelegState::Revoked {
                    self.live_global.fetch_sub(1, Ordering::SeqCst);
                }
                freed.push(rec.stateid);
            }
            drop(e);
            self.gc_entry(ident);
        }
        self.live_per_client.remove(&client_id);
        if !freed.is_empty() {
            info!(
                "deleg: client {} teardown dropped {} delegation record(s)",
                client_id,
                freed.len()
            );
        }
        freed
    }

    /// Snapshot one record (ladder wakeups re-check through this).
    pub fn snapshot(&self, stateid: &StateId) -> Option<DelegSnapshot> {
        let ident = *self.by_stateid.get(&stateid.other)?;
        let entry = self.files.get(&ident)?.clone();
        let e = entry.lock().unwrap();
        e.records
            .iter()
            .find(|r| r.stateid.other == stateid.other)
            .map(|r| DelegSnapshot {
                stateid: r.stateid,
                client_id: r.client_id,
                state: r.state,
                truncate: r.truncate,
            })
        }

    /// Does this client hold any live (non-revoked) delegation?
    /// The holder-evidence persistence and the idle-suspend
    /// `delegations_live` input both key off this.
    pub fn client_holds_live(&self, client_id: u64) -> bool {
        self.live_per_client
            .get(&client_id)
            .map(|v| *v > 0)
            .unwrap_or(false)
    }

    /// Every recorded delegation state for this client, revoked
    /// tombstones INCLUDED — the "does the client still hold state"
    /// question DESTROY_CLIENTID's busy check asks.
    pub fn count_for_client(&self, client_id: u64) -> usize {
        self.by_client
            .get(&client_id)
            .map(|v| v.len())
            .unwrap_or(0)
    }

    /// Does the client still hold any REVOKED tombstone? The SEQ4
    /// RECALLABLE_STATE_REVOKED bit is level-triggered on exactly
    /// this: raised at revocation, lowered when FREE_STATEID drops
    /// the last one.
    pub fn client_has_revoked(&self, client_id: u64) -> bool {
        let others: Vec<[u8; 12]> = match self.by_client.get(&client_id) {
            Some(v) => v.clone(),
            None => return false,
        };
        for other in others {
            let sid = StateId { seqid: 1, other };
            if let Some(snap) = self.snapshot(&sid) {
                if snap.state == DelegState::Revoked {
                    return true;
                }
            }
        }
        false
    }

    /// Live delegations across all clients (metrics; idle input).
    pub fn live_count(&self) -> u64 {
        self.live_global.load(Ordering::SeqCst)
    }

    pub fn grants_total(&self) -> u64 {
        self.grants_total.load(Ordering::SeqCst)
    }

    pub fn refusal_count(&self, reason: GrantRefusal) -> u64 {
        self.refusals
            .get(reason.counter_name())
            .map(|v| *v)
            .unwrap_or(0)
    }

    fn with_record(&self, stateid: &StateId, f: impl FnOnce(&mut DelegRecord)) {
        let ident = match self.by_stateid.get(&stateid.other) {
            Some(i) => *i,
            None => return,
        };
        let entry = match self.files.get(&ident) {
            Some(e) => e.clone(),
            None => return,
        };
        let mut e = entry.lock().unwrap();
        if let Some(r) = e
            .records
            .iter_mut()
            .find(|r| r.stateid.other == stateid.other)
        {
            f(r);
        }
    }

    fn dec_live_client(&self, client_id: u64) {
        if let Some(mut v) = self.live_per_client.get_mut(&client_id) {
            *v = v.saturating_sub(1);
        }
    }

    fn unindex(&self, rec: &DelegRecord) {
        self.by_stateid.remove(&rec.stateid.other);
        if let Some(mut v) = self.by_client.get_mut(&rec.client_id) {
            v.retain(|o| o != &rec.stateid.other);
        }
        if rec.state != DelegState::Revoked {
            self.live_global.fetch_sub(1, Ordering::SeqCst);
            self.dec_live_client(rec.client_id);
        }
        self.gc_entry(rec.ident);
    }

    /// Drop an empty file entry. Cooldown stamps and live guards keep
    /// the entry alive (a guard holds its own Arc, but the map entry
    /// must survive so the NEXT fence/grant sees the same lock).
    fn gc_entry(&self, ident: FileId) {
        gc_map_entry(&self.files, ident);
    }
}

/// DELEGRETURN failure classification (dispatcher maps to Nfs4Status:
/// Unknown → BAD_STATEID, OldSeqid → OLD_STATEID, Revoked →
/// DELEG_REVOKED).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelegReturnError {
    Unknown,
    OldSeqid,
    Revoked,
}

impl Default for DelegationManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sid(n: u8) -> StateId {
        let mut other = [0u8; 12];
        other[0] = n;
        StateId { seqid: 1, other }
    }

    fn mgr() -> DelegationManager {
        DelegationManager::with_limits(4096, 65536, Duration::from_secs(30))
    }

    fn grant(m: &DelegationManager, ident: FileId, client: u64, n: u8) -> StateId {
        m.try_grant(
            ident,
            client,
            vec![n],
            PathBuf::from(format!("/f{}", n)),
            || true,
            || sid(n),
        )
        .expect("grant should succeed")
    }

    const F: FileId = FileId { dev: 1, ino: 100 };

    #[test]
    fn a_granted_delegation_is_visible_and_counted() {
        let m = mgr();
        let s = grant(&m, F, 1, 1);
        assert_eq!(m.lookup(&s), Some((1, PathBuf::from("/f1"))));
        assert_eq!(m.live_count(), 1);
        assert!(m.client_holds_live(1));
        // Two READ delegations on one file across clients coexist.
        grant(&m, F, 2, 2);
        assert_eq!(m.live_count(), 2);
    }

    #[test]
    fn a_live_mutation_guard_blocks_grants_until_dropped() {
        let m = mgr();
        let outcome = m.mutation_fence(F, Some(9), false);
        assert!(matches!(outcome, FenceOutcome::Clear(_)));
        let g = outcome.guard();
        let refused = m.try_grant(F, 1, vec![1], "/f1".into(), || true, || sid(1));
        assert_eq!(refused.unwrap_err(), GrantRefusal::MutationPending);
        assert_eq!(m.refusal_count(GrantRefusal::MutationPending), 1);
        drop(g);
        grant(&m, F, 1, 1);
    }

    #[test]
    fn a_foreign_conflict_recalls_and_delays_single_flight() {
        let m = mgr();
        let s = grant(&m, F, 1, 1);
        // Client 2 mutates: recall client 1's record, DELAY the op.
        match m.mutation_fence(F, Some(2), false) {
            FenceOutcome::Conflict { recalls, delay, .. } => {
                assert!(delay);
                assert_eq!(recalls.len(), 1);
                assert_eq!(recalls[0].stateid, s);
                assert_eq!(recalls[0].client_id, 1);
            }
            FenceOutcome::Clear(_) => panic!("expected conflict"),
        }
        // Second conflictor: still DELAYed, but NO new recall order
        // (single-flight — the record is already RecallPending).
        match m.mutation_fence(F, Some(3), false) {
            FenceOutcome::Conflict { recalls, delay, .. } => {
                assert!(delay);
                assert!(recalls.is_empty());
            }
            FenceOutcome::Clear(_) => panic!("expected conflict"),
        }
    }

    #[test]
    fn the_sole_holder_mutator_is_recalled_but_not_delayed() {
        let m = mgr();
        let s = grant(&m, F, 1, 1);
        match m.mutation_fence(F, Some(1), false) {
            FenceOutcome::Conflict { recalls, delay, .. } => {
                assert!(!delay, "sole holder must not be DELAYed (carve-out)");
                assert_eq!(recalls.len(), 1);
                assert_eq!(recalls[0].stateid, s);
            }
            FenceOutcome::Clear(_) => panic!("carve-out still recalls"),
        }
        // With a SECOND holder, the same mutator IS delayed.
        let m2 = mgr();
        grant(&m2, F, 1, 1);
        grant(&m2, F, 2, 2);
        match m2.mutation_fence(F, Some(1), false) {
            FenceOutcome::Conflict { recalls, delay, .. } => {
                assert!(delay);
                assert_eq!(recalls.len(), 2, "holder A's mutation recalls holder B too");
            }
            FenceOutcome::Clear(_) => panic!("expected conflict"),
        }
    }

    #[test]
    fn a_server_local_mutator_delays_against_every_holder() {
        let m = mgr();
        grant(&m, F, 1, 1);
        match m.mutation_fence(F, None, false) {
            FenceOutcome::Conflict { delay, .. } => assert!(delay),
            FenceOutcome::Clear(_) => panic!("expected conflict"),
        }
    }

    #[test]
    fn return_under_recall_lifts_barrier_and_starts_cooldown() {
        let m = DelegationManager::with_limits(4096, 65536, Duration::from_secs(3600));
        let s = grant(&m, F, 1, 1);
        m.mutation_fence(F, Some(2), true).guard();
        assert_eq!(m.return_delegation(&s), Ok(1));
        assert_eq!(m.lookup(&s), None);
        assert_eq!(m.live_count(), 0);
        // Barrier lifted (no records) — but cooldown now refuses.
        let refused = m.try_grant(F, 3, vec![3], "/f3".into(), || true, || sid(3));
        assert_eq!(refused.unwrap_err(), GrantRefusal::Cooldown);
        // A VOLUNTARY return (no recall) must NOT start cooldown.
        let m2 = mgr();
        let s2 = grant(&m2, F, 1, 1);
        assert_eq!(m2.return_delegation(&s2), Ok(1));
        grant(&m2, F, 3, 3);
    }

    #[test]
    fn delegreturn_seqid_and_unknown_arms() {
        let m = mgr();
        let s = grant(&m, F, 1, 1);
        // seqid 0 (resolved current form) is accepted…
        let mut zero = s;
        zero.seqid = 0;
        assert_eq!(m.return_delegation(&zero), Ok(1));
        // …unknown afterwards.
        assert_eq!(m.return_delegation(&s), Err(DelegReturnError::Unknown));
        // Wrong seqid ⇒ OldSeqid, record NOT dropped.
        let m2 = mgr();
        let s2 = grant(&m2, F, 1, 1);
        let mut wrong = s2;
        wrong.seqid = 7;
        assert_eq!(m2.return_delegation(&wrong), Err(DelegReturnError::OldSeqid));
        assert!(m2.lookup(&s2).is_some());
    }

    #[test]
    fn revoke_requires_a_recall_and_retains_a_grant_blocking_tombstone() {
        let m = DelegationManager::with_limits(4096, 65536, Duration::ZERO);
        let s = grant(&m, F, 1, 1);
        // Granted records are never revoked (revoke-only-from-recall).
        assert_eq!(m.revoke(&s), None);
        m.mutation_fence(F, Some(2), false).guard();
        m.note_first_transmit(&s);
        assert_eq!(m.revoke(&s), Some(1));
        // Idempotent.
        assert_eq!(m.revoke(&s), None);
        assert_eq!(m.live_count(), 0);
        assert!(!m.client_holds_live(1));
        // The tombstone blocks re-grants on the file (Barrier), with
        // cooldown zeroed so it's the tombstone arm we're testing.
        let refused = m.try_grant(F, 3, vec![3], "/f3".into(), || true, || sid(3));
        assert_eq!(refused.unwrap_err(), GrantRefusal::Barrier);
        // DELEGRETURN on the tombstone answers Revoked, retained.
        assert_eq!(m.return_delegation(&s), Err(DelegReturnError::Revoked));
        // FREE_STATEID drops it; grants flow again.
        assert!(m.free_revoked(&s));
        assert!(!m.free_revoked(&s));
        grant(&m, F, 3, 3);
    }

    #[test]
    fn recall_ack_then_return_completes_the_ladder_happy_path() {
        let m = DelegationManager::with_limits(4096, 65536, Duration::ZERO);
        let s = grant(&m, F, 1, 1);
        match m.mutation_fence(F, Some(2), false) {
            FenceOutcome::Conflict { recalls, .. } => assert_eq!(recalls.len(), 1),
            _ => panic!(),
        }
        m.note_first_transmit(&s);
        m.note_recall_acked(&s);
        assert_eq!(m.snapshot(&s).unwrap().state, DelegState::RecallAcked);
        assert_eq!(m.return_delegation(&s), Ok(1));
        assert_eq!(m.snapshot(&s), None);
    }

    #[test]
    fn client_teardown_drops_all_states_and_lifts_barriers() {
        let m = mgr();
        let s1 = grant(&m, FileId::new(1, 100), 1, 1);
        let s2 = grant(&m, FileId::new(1, 200), 1, 2);
        // Put s1 under recall so teardown crosses a barrier.
        m.mutation_fence(FileId::new(1, 100), Some(2), false).guard();
        let freed = m.cleanup_client_delegations(1);
        assert_eq!(freed.len(), 2);
        assert!(freed.contains(&s1) && freed.contains(&s2));
        assert_eq!(m.live_count(), 0);
        // Barrier gone: a new grant on the once-recalled file works
        // (teardown is not a recall — no cooldown).
        grant(&m, FileId::new(1, 100), 3, 3);
        assert!(m.cleanup_client_delegations(99).is_empty());
    }

    #[test]
    fn quotas_refuse_at_the_limit_never_delay() {
        let m = DelegationManager::with_limits(1, 65536, Duration::from_secs(30));
        grant(&m, F, 1, 1);
        let refused = m.try_grant(FileId::new(1, 200), 1, vec![2], "/f2".into(), || true, || sid(2));
        assert_eq!(refused.unwrap_err(), GrantRefusal::QuotaClient);
        // Global cap.
        let mg = DelegationManager::with_limits(4096, 1, Duration::from_secs(30));
        grant(&mg, F, 1, 1);
        let refused = mg.try_grant(FileId::new(1, 200), 2, vec![2], "/f2".into(), || true, || sid(2));
        assert_eq!(refused.unwrap_err(), GrantRefusal::QuotaGlobal);
        // A revoked tombstone frees the quota slot it held.
        let mr = DelegationManager::with_limits(1, 65536, Duration::ZERO);
        let s = grant(&mr, F, 1, 1);
        mr.mutation_fence(F, Some(2), false).guard();
        mr.revoke(&s);
        grant(&mr, FileId::new(1, 200), 1, 2);
    }

    #[test]
    fn precheck_runs_under_the_lock_and_refuses() {
        let m = mgr();
        let refused = m.try_grant(F, 1, vec![1], "/f1".into(), || false, || sid(1));
        assert_eq!(refused.unwrap_err(), GrantRefusal::Precheck);
        assert_eq!(m.refusal_count(GrantRefusal::Precheck), 1);
        assert_eq!(m.live_count(), 0);
    }

    #[test]
    fn already_holder_is_refused_but_other_files_are_fine() {
        let m = mgr();
        grant(&m, F, 1, 1);
        let refused = m.try_grant(F, 1, vec![9], "/f1".into(), || true, || sid(9));
        assert_eq!(refused.unwrap_err(), GrantRefusal::AlreadyHolder);
        grant(&m, FileId::new(1, 200), 1, 2);
    }

    /// THE FENCE-COMPLETENESS GATE (design §5.2): every production
    /// mutation lane — identified mechanically as a lane that bumps
    /// the F14 change counter — must consult the delegation fence, in
    /// itself or in its named caller, or carry an explicit exemption
    /// with a reason. The prose site inventory drifted once
    /// (perfops.rs:2647 turned out to be test code); this version is
    /// executable, so a NEW mutation lane added without a fence fails
    /// here with instructions instead of shipping the V1-fatal grant
    /// race.
    #[test]
    fn every_f14_bump_lane_is_fenced_or_exempted() {
        #[derive(Debug)]
        #[allow(dead_code)] // Allowed's reason is documentation-in-code
        enum Req {
            /// The lane's own fn must contain a deleg_fence consult.
            FencedHere,
            /// The bump lives in a shared helper; these callers carry
            /// the consult.
            FencedInCallers(&'static [&'static str]),
            /// Exempt, with the reason on record.
            Allowed(&'static str),
        }
        use Req::*;
        let expected: &[((&str, &str), Req)] = &[
            (("nfs/v4/dispatcher.rs", "dispatch_operation_inner"),
             Allowed("MDS proxy-fallback WRITE for striped files — an MDS-posture \
                      lane; slice 5 (FLINT_NFS_DELEGATIONS_PNFS) must fence it \
                      before that flag ships")),
            (("nfs/v4/dispatcher.rs", "handle_layoutcommit"),
             Allowed("MDS-posture lane, same slice-5 obligation as above")),
            (("nfs/v4/operations/fileops.rs", "apply_settable_attrs"),
             FencedInCallers(&["handle_setattr"])),
            (("nfs/v4/operations/fileops.rs", "handle_create"),
             Allowed("v4 CREATE makes non-regular objects only (regular files \
                      come via OPEN); nothing it creates or bumps can hold a \
                      READ delegation, and create-over-existing fails EXIST \
                      without truncating")),
            (("nfs/v4/operations/fileops.rs", "handle_link"), FencedHere),
            (("nfs/v4/operations/fileops.rs", "handle_remove"), FencedHere),
            (("nfs/v4/operations/fileops.rs", "handle_rename"), FencedHere),
            (("nfs/v4/operations/ioops.rs", "handle_open"), FencedHere),
            (("nfs/v4/operations/ioops.rs", "handle_write"), FencedHere),
            (("nfs/v4/operations/perfops.rs", "bump_change_counter"),
             FencedInCallers(&["handle_copy", "handle_clone", "fallocate_current_fh"])),
        ];

        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files: Vec<std::path::PathBuf> = Vec::new();
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for e in std::fs::read_dir(dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().map_or(false, |x| x == "rs") {
                    out.push(p);
                }
            }
        }
        walk(&base, &mut files);

        // Enclosing production fn of every change_counter::bump call.
        let mut found: Vec<(String, String, String)> = Vec::new(); // (rel, fn, text)
        for p in &files {
            let rel = p.strip_prefix(&base).unwrap().to_string_lossy().to_string();
            if rel == "nfs/v4/change_counter.rs" || rel == "nfs/v4/stat_cache.rs" {
                // The counter's own module; the cache's test uses it.
                continue;
            }
            let text = std::fs::read_to_string(p).unwrap();
            let prod = match text.find("mod tests") {
                Some(cut) => &text[..cut],
                None => &text[..],
            };
            let mut at = 0usize;
            while let Some(i) = prod[at..].find("change_counter::bump") {
                let abs = at + i;
                at = abs + 1;
                let line_start = prod[..abs].rfind('\n').map(|x| x + 1).unwrap_or(0);
                if prod[line_start..abs].trim_start().starts_with("//") {
                    continue;
                }
                let fn_name = prod[..abs]
                    .rmatch_indices("fn ")
                    .next()
                    .map(|(j, _)| {
                        prod[j + 3..]
                            .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                            .next()
                            .unwrap_or("?")
                            .to_string()
                    })
                    .unwrap_or_else(|| "?".into());
                found.push((rel.clone(), fn_name, text.clone()));
            }
        }

        // Body of `fn name` in `text` — decl to the next fn decl.
        fn fn_body<'a>(text: &'a str, name: &str) -> &'a str {
            let pats = [format!("fn {}(", name), format!("fn {}<", name)];
            let start = pats
                .iter()
                .filter_map(|p| text.find(p.as_str()))
                .min()
                .unwrap_or_else(|| panic!("fn {} not found", name));
            let rest = &text[start + 4..];
            let end = ["\n    pub fn ", "\n    pub async fn ", "\n    fn ", "\n    async fn ", "\nfn ", "\npub fn "]
                .iter()
                .filter_map(|p| rest.find(p))
                .min()
                .unwrap_or(rest.len());
            &text[start..start + 4 + end]
        }

        let mut unexpected = Vec::new();
        for (rel, fnname, text) in &found {
            let req = expected
                .iter()
                .find(|((f, n), _)| f == rel && n == fnname)
                .map(|(_, r)| r);
            match req {
                None => unexpected.push(format!("{}:{}", rel, fnname)),
                Some(Allowed(_)) => {}
                Some(FencedHere) => {
                    assert!(
                        fn_body(text, fnname).contains("deleg_fence"),
                        "{}:{} is a mutation lane with NO fence consult — add a \
                         deleg_fence call before the mutation (design §5.2)",
                        rel,
                        fnname
                    );
                }
                Some(FencedInCallers(callers)) => {
                    for c in *callers {
                        assert!(
                            fn_body(text, c).contains("deleg_fence"),
                            "{}: caller {} of the {} bump helper lost its fence",
                            rel,
                            c,
                            fnname
                        );
                    }
                }
            }
        }
        assert!(
            unexpected.is_empty(),
            "NEW F14 bump lane(s) with no fence ruling: {:?} — every mutation \
             lane must consult the delegation fence or be exempted here WITH A \
             REASON (design §5.2; the grant race this closes is V1-fatal)",
            unexpected
        );

        // And the inventory itself must not silently shrink — a lane
        // moving or renaming should be re-adjudicated, not vanish.
        for ((f, n), _) in expected {
            assert!(
                found.iter().any(|(rel, fnname, _)| rel == f && fnname == n),
                "expected bump lane {}:{} no longer exists — re-derive the \
                 inventory and update this test",
                f,
                n
            );
        }
    }

    #[test]
    fn hardlink_aliases_are_one_file_by_construction() {
        // Two names, one (dev,ino): the second grant to the same
        // client refuses as AlreadyHolder even though fh/path differ —
        // the invariant fh-keying would have falsified.
        let m = mgr();
        m.try_grant(F, 1, vec![1], "/name-a".into(), || true, || sid(1))
            .unwrap();
        let refused = m.try_grant(F, 1, vec![2], "/name-b".into(), || true, || sid(2));
        assert_eq!(refused.unwrap_err(), GrantRefusal::AlreadyHolder);
    }
}
