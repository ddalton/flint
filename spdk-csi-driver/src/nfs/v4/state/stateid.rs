// StateId Management
//
// StateIds are 128-bit identifiers for all NFSv4 state:
// - OPEN stateids (file opens)
// - LOCK stateids (byte-range locks)
// - DELEGATION stateids (delegations)
//
// StateId Structure:
// - seqid (32 bits): Sequence number, incremented on each state change
// - other (96 bits): Opaque identifier (unique per state)
//
// Lifecycle:
// 1. Client performs OPEN → server returns stateid (seqid=1)
// 2. Client performs operation → server validates stateid
// 3. State changes (LOCK) → server increments seqid
// 4. Client closes → server revokes stateid

use super::super::protocol::StateId;
use crate::state_backend::{StateBackend, StateIdRecord, StateTypeRecord, WriteOp};
use dashmap::DashMap;
use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tracing::{debug, info, warn};

/// Outcome of a seqid-checked CLOSE (F31). The discrimination matters:
/// `OldStateId` maps to NFS4ERR_OLD_STATEID, which the client treats as
/// a benign ordering artifact (refresh and re-evaluate), while
/// `NotFound` maps to NFS4ERR_BAD_STATEID, which detonates a full
/// TEST_STATEID recovery round that stalls the whole session.
#[derive(Debug, PartialEq, Eq)]
pub enum CloseOutcome {
    /// State removed; CLOSE succeeded.
    Closed,
    /// The presented seqid is stale (a newer OPEN bumped it) or the
    /// stateid was already closed moments ago — the CLOSE must not
    /// destroy state, and the client should not run recovery.
    OldStateId,
    /// The stateid was administratively revoked.
    Revoked,
    /// Never issued (or long gone) — genuine BAD_STATEID.
    NotFound,
}

/// How many recently-closed stateids to remember for replay/reorder
/// discrimination. Sized to comfortably cover several RTTs of CLOSE
/// traffic under churn; membership is checked only on the miss path.
const CLOSED_TOMBSTONES: usize = 8192;

/// Special stateid for anonymous/READ-only operations
pub const ANONYMOUS_STATEID: StateId = StateId {
    seqid: 0,
    other: [0; 12],
};

/// Special stateid for READ bypass (all ones)
pub const READ_BYPASS_STATEID: StateId = StateId {
    seqid: 0xFFFFFFFF,
    other: [0xFF; 12],
};

/// Type of state represented by a stateid
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateType {
    /// File open state
    Open,

    /// Byte-range lock state
    Lock,

    /// Delegation state
    Delegation,
}

/// State entry - tracks a single stateid
#[derive(Debug, Clone)]
pub struct StateEntry {
    /// The stateid itself
    pub stateid: StateId,

    /// Type of state
    pub state_type: StateType,

    /// Client that owns this state
    pub client_id: u64,

    /// Current sequence number (matches stateid.seqid)
    pub seqid: u32,

    /// File handle this state is associated with (for opens/locks)
    pub filehandle: Option<Vec<u8>>,

    /// Is this state revoked?
    pub revoked: bool,
}

impl StateEntry {
    /// Snapshot the persisted bits of this state entry for the
    /// [`StateBackend`].
    pub(crate) fn to_record(&self) -> StateIdRecord {
        StateIdRecord {
            other: self.stateid.other,
            seqid: self.seqid,
            state_type: match self.state_type {
                StateType::Open => StateTypeRecord::Open,
                StateType::Lock => StateTypeRecord::Lock,
                StateType::Delegation => StateTypeRecord::Delegation,
            },
            client_id: self.client_id,
            filehandle: self.filehandle.clone(),
            revoked: self.revoked,
        }
    }

    /// Inverse of `to_record`. Used at startup by
    /// [`StateIdManager::load_records`] to repopulate the in-memory
    /// DashMap from a backend snapshot.
    pub(crate) fn from_record(r: StateIdRecord) -> Self {
        let stateid = StateId {
            seqid: r.seqid,
            other: r.other,
        };
        let state_type = match r.state_type {
            StateTypeRecord::Open => StateType::Open,
            StateTypeRecord::Lock => StateType::Lock,
            StateTypeRecord::Delegation => StateType::Delegation,
        };
        Self {
            stateid,
            state_type,
            client_id: r.client_id,
            seqid: r.seqid,
            filehandle: r.filehandle,
            revoked: r.revoked,
        }
    }

    /// Create a new state entry
    pub fn new(
        stateid: StateId,
        state_type: StateType,
        client_id: u64,
        filehandle: Option<Vec<u8>>,
    ) -> Self {
        Self {
            stateid,
            state_type,
            client_id,
            seqid: stateid.seqid,
            filehandle,
            revoked: false,
        }
    }

    /// Increment sequence number (for state changes)
    pub fn increment_seqid(&mut self) -> StateId {
        self.seqid += 1;
        self.stateid.seqid = self.seqid;
        self.stateid
    }

    /// Revoke this state
    pub fn revoke(&mut self) {
        self.revoked = true;
        warn!("StateId {:?} revoked", self.stateid);
    }
}

/// StateId manager - tracks all active stateids
///
/// LOCK-FREE DESIGN using DashMap:
/// - Concurrent stateid lookups and validations without blocking
/// - Lock-free stateid allocation and revocation
/// - Per-stateid granularity for high concurrency
/// - Critical for NFSv4.1+ exactly-once semantics (SEQUENCE operations)
/// First byte of the synthetic `other` on a persisted BREAKER-TRIP
/// marker. Holder-evidence markers use 0xFD; these must not collide,
/// because both are Delegation-typed rows in one table.
pub const BREAKER_MARKER_TAG: u8 = 0xFC;

/// What a state-table load found among the Delegation-typed marker
/// rows. Two different facts with two different consumers, so they are
/// returned separately rather than as one list the caller has to
/// re-classify.
#[derive(Debug, Default)]
pub struct LoadedMarkers {
    /// Clients that held recallable state when the predecessor died.
    pub deleg_holders: Vec<u64>,
    /// Unix seconds at which the grant breaker tripped, if it was
    /// tripped when the predecessor died.
    pub breaker_trip_unix: Option<u64>,
}

pub struct StateIdManager {
    /// Counter for generating unique stateid identifiers (lock-free atomic)
    next_stateid: AtomicU64,

    /// Active stateids (stateid.other → state entry)
    /// We use 'other' as key since seqid changes
    /// DashMap enables lock-free concurrent access
    states: DashMap<[u8; 12], StateEntry>,

    /// Client to stateids mapping (for cleanup)
    /// Lock-free per-client state tracking
    client_states: DashMap<u64, Vec<[u8; 12]>>,

    /// Per-(client, owner, filehandle) open state (RFC 7530 §16.16):
    /// repeated OPENs by the same owner on the same file return the
    /// SAME `stateid.other` with `seqid` bumped, and the share-mask
    /// is updated. Keyed by `(client_id, owner_bytes, fh_bytes)`.
    /// **In-memory only** — not persisted because (a) restart drops
    /// in-flight open state anyway, (b) Linux clients reissue OPEN
    /// after restart via the BADSESSION recovery path, and (c)
    /// persisting open-owner records would require schema bump +
    /// boundary plumbing that's heavier than the win.
    open_states: DashMap<(u64, Vec<u8>, Vec<u8>), OpenState>,
    /// F28: reverse index stateid.other → open_states key, so CLOSE is
    /// a point lookup. The old `iter().find()` full-map scan per CLOSE
    /// was O(live opens) on the hot path — under connection churn the
    /// map outgrew the drain rate, CLOSE replies slipped past the
    /// client RTO, retransmits re-executed as not-found, and the retry
    /// feedback starved the session (F26's bug class, stateid layer).
    open_state_keys: DashMap<[u8; 12], (u64, Vec<u8>, Vec<u8>)>,

    /// Index from filehandle bytes → list of `(client_id, owner)`
    /// pairs that have an open on it. Used by share-deny conflict
    /// detection (RFC 8881 §9.7) to scan all conflicting opens
    /// without iterating the full `open_states` map.
    opens_by_fh: DashMap<Vec<u8>, Vec<(u64, Vec<u8>)>>,

    /// (dev,ino) ↔ fh indices for `file_has_write_open`: the
    /// delegation grant predicate must see write opens on EVERY fh
    /// aliasing an inode — in path-handles mode hardlinks give one
    /// inode many fhs, so an fh-keyed check would falsify the
    /// no-conflicting-opens invariant (design §4 rule 5). Populated by
    /// `record_open` when the caller knows the identity, drained with
    /// `opens_by_fh` on last close.
    fhs_by_ident: DashMap<(u64, u64), Vec<Vec<u8>>>,
    idents_by_fh: DashMap<Vec<u8>, (u64, u64)>,

    /// Persistence target. See `client.rs` for the full rationale;
    /// stateids surviving restart is what prevents `BAD_STATEID` on
    /// the client's next WRITE after an MDS pod roll.
    backend: Arc<dyn StateBackend>,

    /// F31: ring of recently-closed `other` values. A CLOSE (or any
    /// stateful op) presenting one of these is a reorder/replay
    /// artifact → OLD_STATEID, not BAD_STATEID. Bounded FIFO; touched
    /// only on close and on the validation *miss* path, so it adds
    /// nothing to the hot path.
    closed_recently: Mutex<(VecDeque<[u8; 12]>, HashSet<[u8; 12]>)>,
}

/// Per-(client, owner, fh) open state. `share_access` /
/// `share_deny` are the bitmasks the client passed; `verifier` is
/// the EXCLUSIVE4 / EXCLUSIVE4_1 verifier captured on the original
/// create so a retry with a matching verifier returns the same
/// stateid (RFC 8881 §18.16.5).
#[derive(Debug, Clone)]
pub struct OpenState {
    pub stateid_other: [u8; 12],
    pub seqid: u32,
    pub share_access: u32,
    pub share_deny: u32,
    pub verifier: Option<u64>,
}

impl StateIdManager {
    /// Create a new stateid manager backed by `backend`.
    pub fn new(backend: Arc<dyn StateBackend>) -> Self {
        info!("StateIdManager created");

        Self {
            next_stateid: AtomicU64::new(1),
            states: DashMap::new(),
            client_states: DashMap::new(),
            open_states: DashMap::new(),
            open_state_keys: DashMap::new(),
            opens_by_fh: DashMap::new(),
            fhs_by_ident: DashMap::new(),
            idents_by_fh: DashMap::new(),
            backend,
            closed_recently: Mutex::new((
                VecDeque::with_capacity(CLOSED_TOMBSTONES),
                HashSet::with_capacity(CLOSED_TOMBSTONES),
            )),
        }
    }

    /// Record `other` as recently closed (bounded FIFO).
    fn push_tombstone(&self, other: [u8; 12]) {
        let mut guard = self.closed_recently.lock().unwrap();
        let (ring, set) = &mut *guard;
        if set.insert(other) {
            ring.push_back(other);
            if ring.len() > CLOSED_TOMBSTONES {
                if let Some(evicted) = ring.pop_front() {
                    set.remove(&evicted);
                }
            }
        }
    }

    /// Was `other` closed recently? (miss-path only)
    fn recently_closed(&self, other: &[u8; 12]) -> bool {
        self.closed_recently.lock().unwrap().1.contains(other)
    }

    /// Look up an existing open by (client, owner, fh). RFC 7530
    /// §16.16: returns Some when this open-owner already has state
    /// for this file; the dispatcher then bumps the existing
    /// stateid's seqid instead of allocating a new one.
    pub fn find_open(
        &self,
        client_id: u64,
        owner: &[u8],
        fh: &[u8],
    ) -> Option<OpenState> {
        self.open_states
            .get(&(client_id, owner.to_vec(), fh.to_vec()))
            .map(|e| e.clone())
    }

    /// RFC 8881 §9.7 share-reservation conflict check. Returns true
    /// if any *other* open on this filehandle has a deny mask that
    /// conflicts with `requested_access`, or holds an access bit
    /// that this caller's deny mask would prohibit.
    ///
    /// "Same (client, owner)" is exempt — repeated OPENs by the
    /// same owner upgrade their own share-mask without conflicting.
    pub fn share_conflict(
        &self,
        fh: &[u8],
        client_id: u64,
        owner: &[u8],
        requested_access: u32,
        requested_deny: u32,
    ) -> bool {
        let entries = match self.opens_by_fh.get(&fh.to_vec()) {
            Some(e) => e.clone(),
            None => return false,
        };
        for (other_cid, other_owner) in entries {
            if other_cid == client_id && other_owner.as_slice() == owner {
                continue; // same owner — no conflict
            }
            if let Some(state) = self
                .open_states
                .get(&(other_cid, other_owner.clone(), fh.to_vec()))
            {
                // existing's deny vs requested access:
                if state.share_deny & requested_access != 0 {
                    return true;
                }
                // existing's access vs our deny:
                if state.share_access & requested_deny != 0 {
                    return true;
                }
            }
        }
        false
    }

    /// Record-or-update an open. If an existing entry exists for the
    /// `(client_id, owner, fh)` triple and the new share-mask differs,
    /// bump its seqid and merge the mask (RFC 7530 §16.16). If the
    /// mask is unchanged, return the existing stateid without bumping
    /// (RFC 8881 §18.16.4: seqid increments only on state change).
    /// Otherwise allocate a fresh stateid via the normal `allocate`
    /// path and stamp it into `open_states`.
    /// Returns the up-to-date `StateId` to send to the client.
    pub fn record_open(
        &self,
        client_id: u64,
        owner: Vec<u8>,
        fh: Vec<u8>,
        share_access: u32,
        share_deny: u32,
        verifier: Option<u64>,
        ident: Option<(u64, u64)>,
    ) -> StateId {
        let key = (client_id, owner.clone(), fh.clone());
        // Identity index first (before any early return below): the
        // grant predicate must never miss an open because its arm
        // skipped the indexing line.
        self.index_ident(&key.2, ident);
        // F31: the whole decide-and-mutate sequence runs under the
        // open_states ENTRY guard, so two concurrent OPENs for the same
        // key cannot both take the fresh path (the double-allocation
        // silently orphaned the loser's stateid — its CLOSE then came
        // back BAD_STATEID and detonated client recovery). Nesting
        // order open_states → {states, open_state_keys, opens_by_fh}
        // is the same one every other path uses; nothing acquires
        // open_states while holding any of those.
        use dashmap::mapref::entry::Entry;
        match self.open_states.entry(key.clone()) {
            Entry::Occupied(mut occ) => {
                // Stale entry (master gone/revoked — crash artifact):
                // retire it IN PLACE, including its reverse-index entry —
                // leaving that behind lets a late CLOSE of the dead
                // stateid resolve to this key and tear down the fresh
                // successor's entry.
                let cur_other = occ.get().stateid_other;
                let master_alive = self
                    .states
                    .get(&cur_other)
                    .map_or(false, |s| !s.revoked);
                if !master_alive {
                    self.open_state_keys
                        .remove_if(&cur_other, |_, k| *k == key);
                    let stateid =
                        self.allocate(StateType::Open, client_id, Some(fh.clone()));
                    self.open_state_keys.insert(stateid.other, key.clone());
                    occ.insert(OpenState {
                        stateid_other: stateid.other,
                        seqid: stateid.seqid,
                        share_access,
                        share_deny,
                        verifier,
                    });
                    let mut owners =
                        self.opens_by_fh.entry(fh).or_insert_with(Vec::new);
                    if !owners.iter().any(|(c, o)| *c == client_id && *o == owner) {
                        owners.push((client_id, owner));
                    }
                    return stateid;
                }

                // Live entry: bump the seqid on EVERY follow-on OPEN,
                // even when the share-mask is unchanged. The bump is the
                // protocol's only defense against a reordered in-flight
                // CLOSE from the same open-owner (the Linux client keys
                // owners by uid, so every process of a workload shares
                // one): a stale CLOSE(seq=k) arriving after this OPEN
                // advanced the state to k+1 must fail OLD_STATEID
                // instead of destroying state the new opener holds.
                // knfsd bumps on every OPEN for the same reason; I/O
                // ops are immune because the client sends them with the
                // seqid=0 "current stateid" form.
                let e = occ.get_mut();
                e.seqid = e.seqid.wrapping_add(1);
                e.share_access |= share_access;
                e.share_deny |= share_deny;
                let stateid = StateId {
                    seqid: e.seqid,
                    other: e.stateid_other,
                };
                let snap = if let Some(mut master) = self.states.get_mut(&e.stateid_other)
                {
                    master.seqid = e.seqid;
                    master.stateid.seqid = e.seqid;
                    Some(master.clone())
                } else {
                    None
                };
                drop(occ);
                // Persist the bump: a failover that reloads a stale
                // seqid would reject the client's current stateid on
                // every subsequent state-changing op.
                if let Some(snap) = snap {
                    self.persist(&snap);
                }
                stateid
            }
            Entry::Vacant(vac) => {
                // Fresh allocation. Reuses the existing `allocate` path
                // so the master `states` map and `client_states` index
                // stay consistent with everything else.
                let stateid = self.allocate(StateType::Open, client_id, Some(fh.clone()));
                self.open_state_keys.insert(stateid.other, key.clone());
                vac.insert(OpenState {
                    stateid_other: stateid.other,
                    seqid: stateid.seqid,
                    share_access,
                    share_deny,
                    verifier,
                });
                let mut owners = self.opens_by_fh.entry(fh.clone()).or_insert_with(Vec::new);
                if !owners.iter().any(|(c, o)| *c == client_id && *o == owner) {
                    owners.push((client_id, owner));
                }
                drop(owners);
                // RE-INDEX, after the open is visible in `opens_by_fh`.
                //
                // The index at the top of this function covers the early
                // returns; it does NOT survive a concurrent close. A
                // CLOSE of the last open on this fh unindexes the
                // identity, and it decides "last" by looking at
                // `opens_by_fh` — which, between our `index_ident` above
                // and the insert just made, does not yet mention us. So
                // the interleaving
                //
                //   A: index_ident(fh)                 (we are indexed)
                //   B: close → opens_by_fh empty → unindex_ident(fh)
                //   A: insert into opens_by_fh          (we are live)
                //
                // leaves a LIVE write open whose fh is absent from
                // `fhs_by_ident`, so `file_has_write_open` cannot reach
                // it and the delegation grant's rule-5 precheck grants
                // against it — silently. Re-indexing here closes that:
                // any unindex racing us now sees our `opens_by_fh` entry
                // and refuses. `index_ident` is idempotent.
                self.index_ident(&fh, ident);
                stateid
            }
        }
    }

    /// OPEN_DOWNGRADE (RFC 8881 §18.18): replace an open's share
    /// masks with a SUBSET of what previous OPENs established, bump
    /// the seqid, and return the refreshed stateid. The kernel sends
    /// this on partial close of dup'd fds with mixed open modes;
    /// answering NotSupp kicked it into state recovery around every
    /// such close (fsstress-found: layout thrash + MDS-fallback EIO
    /// storms on a healthy fleet).
    ///
    /// Errors per §18.18.3: unknown stateid → BAD_STATEID; requested
    /// masks not a subset of the current ones (or empty access) →
    /// INVAL.
    pub fn downgrade_open(
        &self,
        stateid: &crate::nfs::v4::protocol::StateId,
        share_access: u32,
        share_deny: u32,
    ) -> Result<crate::nfs::v4::protocol::StateId, crate::nfs::v4::protocol::Nfs4Status> {
        use crate::nfs::v4::protocol::{Nfs4Status, StateId};
        // open_states is keyed by (client, owner, fh); the wire gives
        // us only the stateid — scan for the matching entry. Downgrades
        // are rare relative to I/O; O(n) here is fine.
        let key = self
            .open_states
            .iter()
            .find(|e| e.value().stateid_other == stateid.other)
            .map(|e| e.key().clone());
        let Some(key) = key else {
            return Err(Nfs4Status::BadStateId);
        };
        let Some(mut entry) = self.open_states.get_mut(&key) else {
            return Err(Nfs4Status::BadStateId);
        };
        // The new masks must be a subset of the union previous OPENs
        // established, and access must name at least one mode.
        if share_access == 0
            || share_access & !entry.share_access != 0
            || share_deny & !entry.share_deny != 0
        {
            return Err(Nfs4Status::Inval);
        }
        entry.seqid = entry.seqid.wrapping_add(1);
        entry.share_access = share_access;
        entry.share_deny = share_deny;
        let refreshed = StateId { seqid: entry.seqid, other: entry.stateid_other };
        if let Some(mut master) = self.states.get_mut(&entry.stateid_other) {
            master.seqid = entry.seqid;
            master.stateid.seqid = entry.seqid;
        }
        Ok(refreshed)
    }

    /// EXCLUSIVE4 / EXCLUSIVE4_1 retry semantics: if any prior open
    /// on this filehandle was an exclusive create with `verifier`,
    /// return its OpenState. RFC 8881 §18.16.5: a retry with the
    /// same verifier returns the existing stateid; a different
    /// verifier on an existing file returns `NFS4ERR_EXIST`.
    pub fn find_exclusive_match(&self, fh: &[u8], verifier: u64) -> Option<OpenState> {
        let entries = self.opens_by_fh.get(&fh.to_vec()).map(|e| e.clone()).unwrap_or_default();
        for (cid, owner) in entries {
            if let Some(state) = self
                .open_states
                .get(&(cid, owner, fh.to_vec()))
            {
                if state.verifier == Some(verifier) {
                    return Some(state.clone());
                }
            }
        }
        None
    }

    /// Repopulate the in-memory DashMap from a backend snapshot.
    /// Bumps `next_stateid` past the highest persisted id so freshly
    /// allocated stateids never collide.
    pub fn load_records(&self, records: Vec<StateIdRecord>) -> LoadedMarkers {
        let mut max_counter: u64 = 0;
        let mut out = LoadedMarkers::default();
        for r in records {
            // Delegation-typed rows are HOLDER-EVIDENCE MARKERS, not
            // live state (design §6): real delegation stateids are
            // never persisted — they die with the process — and the
            // marker's synthetic `other` would poison the counter
            // recovery below. Divert them to the caller, which
            // pre-arms the client's SEQ4 bit so a restart can never
            // silently orphan a holder's belief.
            if r.state_type == StateTypeRecord::Delegation {
                // Two kinds of Delegation-typed marker share this
                // table, told apart by the first byte of the synthetic
                // `other`. Without the split, the breaker row would be
                // read as a holder marker and pre-arm SEQ4 for a
                // client id that never existed — a revocation notice
                // sent to nobody, and a real one harder to find.
                if r.other[0] == BREAKER_MARKER_TAG {
                    let mut ts = [0u8; 8];
                    ts.copy_from_slice(&r.other[4..12]);
                    out.breaker_trip_unix = Some(u64::from_be_bytes(ts));
                } else {
                    out.deleg_holders.push(r.client_id);
                }
                continue;
            }
            // Recover the numeric counter from the high 8 bytes of
            // `other` — `allocate` encodes `(counter, client_id_low)`
            // there.
            let mut buf = [0u8; 8];
            buf.copy_from_slice(&r.other[0..8]);
            max_counter = max_counter.max(u64::from_be_bytes(buf));
            let cid = r.client_id;
            let other = r.other;
            self.states.insert(other, StateEntry::from_record(r));
            self.client_states
                .entry(cid)
                .or_insert_with(Vec::new)
                .push(other);
        }
        if max_counter >= self.next_stateid.load(Ordering::SeqCst) {
            self.next_stateid.store(max_counter + 1, Ordering::SeqCst);
        }
        info!(
            "StateIdManager loaded {} records from backend ({} delegation holder markers)",
            self.states.len(),
            out.deleg_holders.len(),
        );
        out
    }

    /// Ordered capture at the mutation site (F27): an OPEN's put and a
    /// fast-following CLOSE's delete reach the backend in call order,
    /// so the delete can never be overwritten by a late put.
    fn persist(&self, e: &StateEntry) {
        self.backend.enqueue_write(WriteOp::PutStateid(e.to_record()));
    }

    fn persist_delete(&self, other: [u8; 12]) {
        self.backend.enqueue_write(WriteOp::DeleteStateid(other));
    }

    /// Allocate a new stateid
    ///
    /// LOCK-FREE: Concurrent allocations use atomic counter + per-shard DashMap locks
    pub fn allocate(
        &self,
        state_type: StateType,
        client_id: u64,
        filehandle: Option<Vec<u8>>,
    ) -> StateId {
        // Generate unique identifier (lock-free atomic)
        let id = self.next_stateid.fetch_add(1, Ordering::SeqCst);

        // Build 'other' field (96 bits = 12 bytes)
        let mut other = [0u8; 12];
        other[0..8].copy_from_slice(&id.to_be_bytes());
        other[8..12].copy_from_slice(&(client_id as u32).to_be_bytes());

        // Create stateid with seqid=1
        let stateid = StateId {
            seqid: 1,
            other,
        };

        // Create state entry
        let entry = StateEntry::new(stateid, state_type, client_id, filehandle);

        // Persist before inserting so the boundary code in B.4 sees a
        // consistent snapshot if list_stateids is called concurrently
        // with allocations (it isn't today, but cheap insurance).
        self.persist(&entry);

        // LOCK-FREE: Direct DashMap inserts without global locks
        self.states.insert(other, entry);
        self.client_states
            .entry(client_id)
            .or_insert_with(Vec::new)
            .push(other);

        debug!("StateId allocated: {:?} for client {} (type: {:?})",
              stateid, client_id, state_type);
        stateid
    }

    /// Mint a DELEGATION stateid: same counter and maps as
    /// `allocate`, with two deliberate differences (design §6).
    ///
    /// 1. NOT persisted — delegations die with the process by design
    ///    (holder EVIDENCE persists separately); writing rows the
    ///    startup path must then convert to tombstones would bloat
    ///    the DB for state that is defined to evaporate.
    /// 2. A per-boot epoch is mixed into the `other` bytes.
    ///    Unpersisted mints advance the in-memory counter PAST the
    ///    persisted high-watermark, so after a restart the counter
    ///    re-issues ids still live in some holder's memory; without
    ///    the epoch, a pre-restart delegation stateid could equal a
    ///    post-restart OPEN stateid byte-for-byte and validate as it.
    pub fn allocate_delegation(&self, client_id: u64, filehandle: Vec<u8>) -> StateId {
        static BOOT_EPOCH: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
        let epoch = *BOOT_EPOCH.get_or_init(|| {
            let t = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0);
            t ^ std::process::id()
        });
        let id = self.next_stateid.fetch_add(1, Ordering::SeqCst);
        let mut other = [0u8; 12];
        other[0..8].copy_from_slice(&id.to_be_bytes());
        other[8..12].copy_from_slice(&((client_id as u32) ^ epoch).to_be_bytes());
        let stateid = StateId { seqid: 1, other };
        let entry = StateEntry::new(stateid, StateType::Delegation, client_id, Some(filehandle));
        self.states.insert(other, entry);
        self.client_states
            .entry(client_id)
            .or_insert_with(Vec::new)
            .push(other);
        debug!(
            "Delegation stateid allocated: {:?} for client {} (unpersisted, epoch-mixed)",
            stateid, client_id
        );
        stateid
    }

    /// Validate a stateid for any state-using operation (WRITE, OPEN_DOWNGRADE,
    /// CLOSE, LOCK, …).
    ///
    /// RFC 8881 §16.2.3.1 / §8.2.2 admits four forms:
    ///   * `ANONYMOUS_STATEID` (all zeros)
    ///   * `READ_BYPASS_STATEID` (all 0xFF)
    ///   * "Current stateid" form: `seqid == 0` with a non-zero `other`. The
    ///     server MUST resolve this to the most recent seqid for that
    ///     `other`. Linux clients carry seqid=0 routinely after an OPEN
    ///     because the open-response seqid doesn't propagate to subsequent
    ///     ops in the same COMPOUND.
    ///   * Exact match of the server's current `seqid` for `other`.
    ///
    /// We do NOT accept `seqid - 1` here — that's a READ-only relaxation
    /// (see `validate_for_read`).
    /// True only when this stateid EXISTS and was established by a
    /// DIFFERENT client.
    ///
    /// A miss deliberately answers false. CLOSE's replay and tombstone
    /// paths depend on an unknown stateid falling through to them, and
    /// turning a benign reorder into BAD_STATEID detonates a
    /// TEST_STATEID recovery round that stalls the whole session — the
    /// F31 lesson, which is why `close_open` discriminates outcomes at
    /// all. Ownership is a separate question from existence; this
    /// answers only ownership.
    pub fn belongs_to_other_client(&self, stateid: &StateId, client_id: u64) -> bool {
        if stateid == &ANONYMOUS_STATEID || stateid == &READ_BYPASS_STATEID {
            return false;
        }
        self.states
            .get(&stateid.other)
            .map(|e| e.client_id != client_id)
            .unwrap_or(false)
    }

    /// True when this stateid names a DELEGATION entry. A READ delegation
    /// stateid presented on WRITE is an access-mode violation — RFC 8881
    /// §18.32.3 / §8.2.4 requires NFS4ERR_OPENMODE, and no access-mode
    /// check existed anywhere before this (the misuse validated cleanly).
    /// WRITE delegations are never granted, so every delegation entry is
    /// read-mode; if that ever changes this check must consult the mode.
    /// (docs/plans/nfs-delegations-design.md, slice 1.)
    pub fn is_delegation(&self, stateid: &StateId) -> bool {
        if stateid == &ANONYMOUS_STATEID || stateid == &READ_BYPASS_STATEID {
            return false;
        }
        self.states
            .get(&stateid.other)
            .map(|e| e.state_type == StateType::Delegation)
            .unwrap_or(false)
    }

    /// The status to answer when validation has just failed for this
    /// stateid.
    ///
    /// RFC 8881 §8.2.4: a stateid whose state was REVOKED does not get
    /// NFS4ERR_BAD_STATEID. The two send a client down different
    /// recovery paths — BAD_STATEID says "this was never yours", so the
    /// client discards it as a protocol error, while DELEG_REVOKED says
    /// "you held this and the server took it back", which is what tells
    /// the holder its cached data is no longer authoritative and must
    /// be re-fetched. Answering BAD_STATEID to a revoked delegation is
    /// therefore not a cosmetic error-code difference: it is the server
    /// declining to say the one thing that makes a revocation safe.
    ///
    /// Deliberately narrow. Only a revoked DELEGATION maps to
    /// DELEG_REVOKED; a revoked open or lock stateid keeps today's
    /// answer, because their correct codes (EXPIRED vs ADMIN_REVOKED)
    /// depend on WHY they were revoked, and this manager does not
    /// record that yet. Guessing would trade one wrong code for
    /// another.
    ///
    /// Found by pynfs DELEG8: "Read with a revoked delegation should
    /// return NFS4ERR_DELEG_REVOKED, instead got NFS4ERR_BAD_STATEID".
    pub fn invalid_status(&self, stateid: &StateId) -> crate::nfs::v4::protocol::Nfs4Status {
        use crate::nfs::v4::protocol::Nfs4Status;
        match self.states.get(&stateid.other) {
            Some(e) if e.revoked && e.state_type == StateType::Delegation => {
                Nfs4Status::DelegRevoked
            }
            _ => Nfs4Status::BadStateId,
        }
    }

    pub fn validate(&self, stateid: &StateId) -> Result<(), String> {
        if stateid == &ANONYMOUS_STATEID {
            return Ok(());
        }
        if stateid == &READ_BYPASS_STATEID {
            return Ok(());
        }

        let entry = self.states.get(&stateid.other)
            .ok_or_else(|| "StateId not found".to_string())?;
        if entry.revoked {
            return Err("StateId revoked".to_string());
        }

        // "Current stateid" form: seqid=0 with a non-zero other resolves to
        // the latest seqid the server holds for this state.
        if stateid.seqid == 0 {
            return Ok(());
        }

        if stateid.seqid == entry.seqid {
            Ok(())
        } else {
            Err(format!("StateId sequence mismatch: expected {} (or 0 for current), got {}",
                       entry.seqid, stateid.seqid))
        }
    }

    /// Validate a stateid for READ.
    ///
    /// RFC 5661 §8.2.2 / §18.22: READ accepts a few stateid forms:
    ///   * `ANONYMOUS_STATEID` (all zeros) — anonymous read.
    ///   * `READ_BYPASS_STATEID` (all 0xFF) — bypass file locking checks.
    ///   * An open / lock / delegation stateid whose `seqid` matches the
    ///     server's current value, OR is the immediately preceding value
    ///     (the client may legitimately race a SETATTR/OPEN_DOWNGRADE
    ///     against READ — RFC 5661 §8.2.2 specifically allows the
    ///     "current" stateid OR `seqid - 1`).
    ///
    /// The previous implementation also accepted any unknown stateid with
    /// `seqid == 0` and any seqid mismatch with a `warn!()` — both are RFC
    /// violations that hide client bugs.
    pub fn validate_for_read(&self, stateid: &StateId) -> Result<(), String> {
        if stateid == &ANONYMOUS_STATEID {
            return Ok(());
        }
        if stateid == &READ_BYPASS_STATEID {
            return Ok(());
        }

        let entry = self.states.get(&stateid.other)
            .ok_or_else(|| "StateId not found".to_string())?;
        if entry.revoked {
            return Err("StateId revoked".to_string());
        }

        // "Current stateid" form (RFC 8881 §8.2.2): seqid=0 with a known
        // `other` means "most recent seqid" and MUST NOT be seqid-checked.
        // `validate` had this arm; this fn missed it — the kernel client
        // retries READ with seqid=0 in a tight loop when the answer is
        // BadStateId (observed wedging pg bring-up on u11.9).
        if stateid.seqid == 0 {
            return Ok(());
        }

        // Accept exact seqid, or seqid - 1 (a one-behind retransmit window).
        let prev = entry.seqid.saturating_sub(1);
        if stateid.seqid == entry.seqid || stateid.seqid == prev {
            Ok(())
        } else {
            Err(format!(
                "StateId seqid mismatch: expected {} (or {}), got {}",
                entry.seqid, prev, stateid.seqid
            ))
        }
    }

    /// Update a stateid's sequence number (for state changes)
    ///
    /// LOCK-FREE: Per-stateid locking only, not global
    pub fn update_seqid(&self, stateid: &StateId) -> Result<StateId, String> {
        let result = if let Some(mut entry) = self.states.get_mut(&stateid.other) {
            if entry.revoked {
                return Err("Cannot update revoked stateid".to_string());
            }
            let new_id = entry.increment_seqid();
            let snap = entry.clone();
            drop(entry);
            self.persist(&snap);
            new_id
        } else {
            return Err("StateId not found".to_string());
        };
        Ok(result)
    }

    /// Get state entry
    ///
    /// LOCK-FREE: Concurrent reads without blocking
    pub fn get_state(&self, stateid: &StateId) -> Option<StateEntry> {
        self.states.get(&stateid.other).map(|r| r.clone())
    }

    /// Revoke a stateid (e.g., for CLOSE operation)
    ///
    /// LOCK-FREE: Per-stateid locking only, not global
    pub fn revoke(&self, stateid: &StateId) -> Result<(), String> {
        if let Some(mut entry) = self.states.get_mut(&stateid.other) {
            entry.revoke();
            let snap = entry.clone();
            drop(entry);
            self.persist(&snap);
            Ok(())
        } else {
            Err("StateId not found".to_string())
        }
    }

    /// Remove a stateid completely (cleanup)
    ///
    /// LOCK-FREE: Removal only locks specific shards, not entire map
    pub fn remove(&self, stateid: &StateId) {
        if let Some((_, entry)) = self.states.remove(&stateid.other) {
            // Remove from client mapping (per-client locking only)
            if let Some(mut state_list) = self.client_states.get_mut(&entry.client_id) {
                state_list.retain(|other| other != &stateid.other);
            }

            self.persist_delete(stateid.other);

            debug!("StateId removed: {:?}", stateid);
        }
    }

    /// Seqid-checked CLOSE (F31). Atomically verifies that the presented
    /// stateid is the CURRENT one for its open (via `remove_if` under the
    /// entry's shard lock — the same lock `record_open`'s merge path
    /// holds while bumping) and only then tears the state down. A stale
    /// seqid means a newer OPEN by the same owner advanced the state
    /// after this CLOSE was sent: report `OldStateId` and destroy
    /// nothing — the newer opener legitimately holds this stateid.
    /// Record fh → (dev,ino). An fh re-binding to a DIFFERENT inode
    /// (file replaced under a path handle) migrates between ident
    /// buckets. Never holds one ident-map guard while acquiring the
    /// other map's same entry twice; callers must not hold either map.
    fn index_ident(&self, fh: &[u8], ident: Option<(u64, u64)>) {
        let Some(ident) = ident else { return };
        use dashmap::mapref::entry::Entry;
        match self.idents_by_fh.entry(fh.to_vec()) {
            Entry::Occupied(mut occ) => {
                let old = *occ.get();
                if old == ident {
                    return;
                }
                occ.insert(ident);
                drop(occ);
                if let Some(mut v) = self.fhs_by_ident.get_mut(&old) {
                    v.retain(|f| f != fh);
                    let empty = v.is_empty();
                    drop(v);
                    if empty {
                        self.fhs_by_ident.remove_if(&old, |_, v| v.is_empty());
                    }
                }
                let mut v = self.fhs_by_ident.entry(ident).or_insert_with(Vec::new);
                if !v.iter().any(|f| f == fh) {
                    v.push(fh.to_vec());
                }
            }
            Entry::Vacant(vac) => {
                vac.insert(ident);
                let mut v = self.fhs_by_ident.entry(ident).or_insert_with(Vec::new);
                if !v.iter().any(|f| f == fh) {
                    v.push(fh.to_vec());
                }
            }
        }
    }

    /// Drop fh from the identity indices (last open on it closed).
    ///
    /// Refuses if the fh has opens again: a concurrent `record_open`
    /// may have re-populated it between the caller's removal and this
    /// call, and dropping the identity index for a LIVE open is the
    /// unsafe direction — the grant predicate reads `fhs_by_ident` and
    /// would miss it. A STALE entry, by contrast, costs one wasted
    /// lookup and can only make the predicate more conservative.
    ///
    /// This narrows the window rather than closing it: the durable fix
    /// is a write-open COUNT keyed on (dev,ino), maintained by
    /// `record_open`/`close_open`, so the predicate is one atomic read
    /// with no multi-map interval at all. Left for the delegation
    /// workstream, which owns this invariant.
    fn unindex_ident(&self, fh: &[u8]) {
        if self.opens_by_fh.contains_key(fh) {
            return;
        }
        if let Some((_, ident)) = self.idents_by_fh.remove(fh) {
            if let Some(mut v) = self.fhs_by_ident.get_mut(&ident) {
                v.retain(|f| f != fh);
                let empty = v.is_empty();
                drop(v);
                if empty {
                    self.fhs_by_ident.remove_if(&ident, |_, v| v.is_empty());
                }
            }
        }
    }

    /// Does ANY client hold a write-capable open on this inode, on any
    /// fh aliasing it? The delegation grant's rule-5 predicate (design
    /// §4). Clones the small index vectors up front so no map guard is
    /// held while other maps are read — `record_open` acquires in the
    /// opposite direction and a held guard here would be a lock cycle.
    pub fn file_has_write_open(&self, dev: u64, ino: u64) -> bool {
        let fhs: Vec<Vec<u8>> = match self.fhs_by_ident.get(&(dev, ino)) {
            Some(v) => v.clone(),
            None => return false,
        };
        for fh in fhs {
            let owners: Vec<(u64, Vec<u8>)> = match self.opens_by_fh.get(&fh) {
                Some(o) => o.clone(),
                None => continue,
            };
            for (client_id, owner) in owners {
                if let Some(st) = self.open_states.get(&(client_id, owner, fh.clone())) {
                    if st.share_access & 2 != 0 {
                        return true;
                    }
                }
            }
        }
        false
    }

    pub fn close_open(&self, stateid: &StateId) -> CloseOutcome {
        let other = &stateid.other;
        let Some(key) = self.open_state_keys.get(other).map(|e| e.value().clone()) else {
            // No open record. Recently closed → benign replay/reorder.
            if self.recently_closed(other) {
                return CloseOutcome::OldStateId;
            }
            return match self.states.get(other) {
                Some(e) if e.revoked => CloseOutcome::Revoked,
                // Master exists with no open record (crash artifact or
                // non-open stateid handed to CLOSE) — treat as unknown.
                _ => CloseOutcome::NotFound,
            };
        };

        // Pre-announce the tombstone BEFORE the removal: a concurrent
        // duplicate/stale close that loses the race reads these maps in
        // the instruction window between our remove_if and any
        // post-removal bookkeeping — it must find the tombstone already
        // in place or it misreports a benign reorder as BAD_STATEID.
        // Harmless when we end up NOT removing (tombstones are only
        // consulted on miss paths, where the stateid is genuinely gone).
        self.push_tombstone(*other);

        // Atomic check-and-remove: the predicate runs under the shard
        // lock, so it cannot interleave with a concurrent merge bump or
        // a fresh insert that re-used this key for a successor stateid.
        let removed = self.open_states.remove_if(&key, |_, v| {
            v.stateid_other == *other
                && (stateid.seqid == 0 || stateid.seqid == v.seqid)
        });

        if removed.is_none() {
            // Re-read to say why. All of these are race-tolerant
            // best-effort reporting; nothing is mutated here.
            if let Some(v) = self.open_states.get(&key) {
                if v.stateid_other == *other {
                    return CloseOutcome::OldStateId; // live, newer seqid
                }
                // Key re-used by a successor stateid; this one is done.
                return if self.recently_closed(other) {
                    CloseOutcome::OldStateId
                } else {
                    CloseOutcome::NotFound
                };
            }
            return if self.recently_closed(other) {
                CloseOutcome::OldStateId
            } else {
                CloseOutcome::NotFound
            };
        }

        self.finish_close(other, &key);
        CloseOutcome::Closed
    }

    /// Force-close: seqid-agnostic removal, used by FREE_STATEID on
    /// revoked state and by tests. `remove_if` still guards against
    /// tearing down a SUCCESSOR open that re-used the same key after
    /// this stateid was already closed.
    pub fn close_open_state(&self, other: &[u8; 12]) {
        self.push_tombstone(*other);
        let key = self.open_state_keys.get(other).map(|e| e.value().clone());
        if let Some(key) = key {
            if self
                .open_states
                .remove_if(&key, |_, v| v.stateid_other == *other)
                .is_some()
            {
                self.finish_close(other, &key);
                return;
            }
            // Open entry gone or superseded — still retire the master
            // record and the index entry for THIS stateid.
            self.open_state_keys.remove_if(other, |_, k| *k == key);
        }
        self.remove_master(other);
        self.push_tombstone(*other);
    }

    /// Shared teardown after the open_states entry for (`other`, `key`)
    /// has been removed: tombstone FIRST (a concurrent duplicate close
    /// that lost the `remove_if` race consults it immediately), then
    /// reverse index, by-fh index, master record, persistence.
    fn finish_close(&self, other: &[u8; 12], key: &(u64, Vec<u8>, Vec<u8>)) {
        self.push_tombstone(*other);
        self.open_state_keys.remove_if(other, |_, k| k == key);
        let (client_id, ref owner, ref fh) = *key;
        if let Some(mut owners) = self.opens_by_fh.get_mut(fh) {
            owners.retain(|(cid, own)| *cid != client_id || own != owner);
            let now_empty = owners.is_empty();
            drop(owners);
            // CHECK-THEN-ACT, and the guard is gone. `remove_if`
            // evaluates the predicate under the shard lock, so a
            // concurrent `record_open` that re-populated this fh in the
            // gap WINS instead of having its entry deleted wholesale.
            // Deleting it was a false NEGATIVE for
            // `file_has_write_open`, which is the unsafe direction: the
            // delegation grant predicate would miss a live write open
            // and rule 5 would be violated with no error anywhere.
            if now_empty && self.opens_by_fh.remove_if(fh, |_, v| v.is_empty()).is_some() {
                self.unindex_ident(fh);
            }
        }
        self.remove_master(other);
    }

    /// Remove the master `states` record + client index + persistence.
    fn remove_master(&self, other: &[u8; 12]) {
        if let Some((_, entry)) = self.states.remove(other) {
            if let Some(mut state_list) = self.client_states.get_mut(&entry.client_id) {
                state_list.retain(|o| o != other);
            }
            self.persist_delete(*other);
        }
    }

    /// Stateids this client holds — the number the
    /// `max_stateids_per_client` quota is checked against. O(1): reads
    /// the length of the maintained per-client index.
    pub fn count_for_client(&self, client_id: u64) -> usize {
        self.client_states
            .get(&client_id)
            .map(|l| l.len())
            .unwrap_or(0)
    }

    /// Get all stateids for a client
    ///
    /// LOCK-FREE: Concurrent reads without blocking
    pub fn get_client_stateids(&self, client_id: u64) -> Vec<StateId> {
        if let Some(state_list) = self.client_states.get(&client_id) {
            state_list
                .iter()
                .filter_map(|other| self.states.get(other).map(|e| e.stateid))
                .collect()
        } else {
            Vec::new()
        }
    }

    /// Revoke all stateids for a client (for client cleanup)
    ///
    /// LOCK-FREE: Uses lock-free get and revoke operations
    pub fn revoke_client_stateids(&self, client_id: u64) {
        let stateids = self.get_client_stateids(client_id);

        for stateid in stateids {
            let _ = self.revoke(&stateid);
        }

        info!("All stateids revoked for client {}", client_id);
    }

    /// Remove all stateids for a client (cleanup). Also drops the
    /// client's open-state records and removes them from the
    /// `opens_by_fh` index so a fresh conflicting OPEN can succeed
    /// (RFC 8881 §8.4.2.4 courtesy-release semantics: when a
    /// client's lease expires, its share-reservations become
    /// courtesy state and MUST be released on conflict from another
    /// client).
    ///
    /// LOCK-FREE: Uses lock-free removal operations
    pub fn remove_client_stateids(&self, client_id: u64) {
        if let Some(state_list) = self.client_states.get(&client_id).map(|r| r.clone()) {
            let count = state_list.len();

            // Remove all stateids (each removal only locks its shard)
            for other in &state_list {
                self.states.remove(other);
                self.persist_delete(*other);
            }

            // Remove client mapping
            self.client_states.remove(&client_id);

            info!("Removed {} stateids for client {}", count, client_id);
        }

        // Open-state cleanup: every (client_id, owner, fh) entry
        // belonging to this client AND its index entries in
        // `opens_by_fh`. Iterating once and rebuilding is cheaper
        // than retain() since we touch both maps.
        let to_drop: Vec<(u64, Vec<u8>, Vec<u8>)> = self
            .open_states
            .iter()
            .filter(|e| e.key().0 == client_id)
            .map(|e| e.key().clone())
            .collect();
        for key in &to_drop {
            self.open_states.remove(key);
        }
        // Strip the client out of the by-fh index. If the resulting
        // entry is empty, drop the fh key too.
        let touched_fhs: Vec<Vec<u8>> = to_drop.iter().map(|k| k.2.clone()).collect();
        for fh in touched_fhs {
            if let Some(mut entry) = self.opens_by_fh.get_mut(&fh) {
                entry.retain(|(cid, _)| *cid != client_id);
                let now_empty = entry.is_empty();
                drop(entry);
                if now_empty && self.opens_by_fh.remove_if(&fh, |_, v| v.is_empty()).is_some() {
                    self.unindex_ident(&fh);
                }
            }
        }
    }

    /// Get total active stateid count
    ///
    /// LOCK-FREE: Counts without blocking concurrent operations
    pub fn active_count(&self) -> usize {
        self.states.len()
    }

    /// Get stateid count by type
    ///
    /// LOCK-FREE: Iterates without blocking concurrent operations
    pub fn count_by_type(&self, state_type: StateType) -> usize {
        self.states
            .iter()
            .filter(|e| e.value().state_type == state_type && !e.value().revoked)
            .count()
    }
}

// `StateIdManager` no longer has a `Default` impl: see `SessionManager`
// for the rationale (constructor now requires a backend).

#[cfg(test)]
mod tests {
    use super::*;

    /// Two kinds of Delegation-typed marker share the stateid table.
    /// If the loader could not tell them apart, the breaker row would
    /// be read as a holder marker for `client_id: 0` and the restart
    /// path would pre-arm SEQ4_STATUS_RECALLABLE_STATE_REVOKED for a
    /// client that never existed — a revocation notice addressed to
    /// nobody, and a real one harder to find among the noise.
    #[test]
    fn a_breaker_marker_is_not_mistaken_for_a_holder_marker() {
        let mgr = StateIdManager::new(crate::state_backend::memory_backend());

        let mut holder = [0xFDu8; 12];
        holder[4..12].copy_from_slice(&7u64.to_be_bytes());
        let mut breaker = [BREAKER_MARKER_TAG; 12];
        breaker[4..12].copy_from_slice(&1_700_000_000u64.to_be_bytes());

        let out = mgr.load_records(vec![
            StateIdRecord {
                other: holder,
                seqid: 0,
                state_type: StateTypeRecord::Delegation,
                client_id: 7,
                filehandle: None,
                revoked: true,
            },
            StateIdRecord {
                other: breaker,
                seqid: 0,
                state_type: StateTypeRecord::Delegation,
                client_id: 0,
                filehandle: None,
                revoked: true,
            },
        ]);

        assert_eq!(out.deleg_holders, vec![7], "only the real holder is a holder");
        assert_eq!(out.breaker_trip_unix, Some(1_700_000_000));
        // And neither marker may leak into live stateid state: the
        // synthetic `other` would poison the counter recovery.
        assert!(mgr.validate(&StateId { seqid: 0, other: holder }).is_err());
        assert!(mgr.validate(&StateId { seqid: 0, other: breaker }).is_err());
    }

    #[test]
    fn test_stateid_allocation() {
        let mgr = StateIdManager::new(crate::state_backend::memory_backend());

        let stateid1 = mgr.allocate(StateType::Open, 1, None);
        let stateid2 = mgr.allocate(StateType::Open, 1, None);

        // Should be unique
        assert_ne!(stateid1.other, stateid2.other);

        // Both should start with seqid=1
        assert_eq!(stateid1.seqid, 1);
        assert_eq!(stateid2.seqid, 1);

        assert_eq!(mgr.active_count(), 2);
    }

    /// RFC 8881 §8.2.4: a REVOKED delegation answers DELEG_REVOKED, not
    /// BAD_STATEID.
    ///
    /// The distinction is the whole safety story of a revocation.
    /// BAD_STATEID tells the client "this was never yours", so it drops
    /// the stateid as a protocol error and keeps whatever it cached
    /// under it. DELEG_REVOKED tells it "you held this and the server
    /// took it back", which is the signal that its cached data is no
    /// longer authoritative. Answering the first to a holder that is
    /// still serving reads from its page cache is how a delegation
    /// becomes stale-forever — the exact failure the whole recall
    /// design exists to prevent.
    ///
    /// Found in the field by pynfs DELEG8, not by any unit test here.
    #[test]
    fn a_revoked_delegation_answers_deleg_revoked_not_bad_stateid() {
        use crate::nfs::v4::protocol::Nfs4Status;
        let mgr = StateIdManager::new(crate::state_backend::memory_backend());

        let deleg = mgr.allocate(StateType::Delegation, 1, None);
        // Precondition: while it is LIVE it validates, so the assertion
        // below is about revocation and not about a stateid that was
        // always broken.
        assert!(mgr.validate_for_read(&deleg).is_ok());
        mgr.revoke(&deleg).unwrap();
        assert!(
            mgr.validate_for_read(&deleg).is_err(),
            "precondition: a revoked stateid must fail validation at all",
        );
        assert_eq!(mgr.invalid_status(&deleg), Nfs4Status::DelegRevoked);

        // A revoked OPEN keeps today's answer on purpose: its correct
        // code is EXPIRED or ADMIN_REVOKED depending on WHY, and this
        // manager does not record why. Guessing would swap one wrong
        // code for another.
        let open = mgr.allocate(StateType::Open, 1, None);
        mgr.revoke(&open).unwrap();
        assert_eq!(mgr.invalid_status(&open), Nfs4Status::BadStateId);

        // An unknown stateid is a bad stateid, and a LIVE delegation
        // must not be reported as revoked — this is consulted only
        // after a failure (a seqid mismatch, say), and it must not
        // manufacture a revocation that never happened.
        assert_eq!(
            mgr.invalid_status(&StateId { seqid: 1, other: [0x5a; 12] }),
            Nfs4Status::BadStateId,
        );
        let live = mgr.allocate(StateType::Delegation, 2, None);
        assert_eq!(mgr.invalid_status(&live), Nfs4Status::BadStateId);
    }

/// The part of a source file that is NOT the trailing `mod tests`.
    ///
    /// Cutting at the first `#[cfg(test)]` is WRONG and was wrong here: a
    /// `#[cfg(test)]` attribute on a single production helper appears at
    /// ioops.rs:459, so a scan that stopped there covered 459 lines of a
    /// 4000-line file and its red proof passed. Cut only at the attribute
    /// that actually introduces a module.
    fn production_source(src: &str) -> &str {
        let lines: Vec<&str> = src.lines().collect();
        let mut off = 0usize;
        for (i, l) in lines.iter().enumerate() {
            if l.trim() == "#[cfg(test)]" {
                let next = lines[i + 1..]
                    .iter()
                    .find(|x| !x.trim().is_empty())
                    .map(|x| x.trim_start())
                    .unwrap_or("");
                if next.starts_with("mod ") || next.starts_with("pub mod ") {
                    return &src[..off];
                }
            }
            off += l.len() + 1;
        }
        src
    }

    /// No stateid-validating op may hardcode BAD_STATEID again.
    ///
    /// Every one of these ten sites was written the same way, by
    /// copying the one before it — which is exactly how the next op
    /// added will be written. A reviewer cannot see the omission,
    /// because the wrong code is the plausible one.
    #[test]
    fn no_read_validation_site_hardcodes_bad_stateid() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut offenders = Vec::new();
        for rel in ["src/nfs/v4/operations/ioops.rs", "src/nfs/v4/operations/perfops.rs"] {
            let src = std::fs::read_to_string(root.join(rel)).unwrap();
            let production = production_source(&src);
            let lines: Vec<&str> = production.lines().collect();
            for (i, l) in lines.iter().enumerate() {
                if !(l.contains("validate_for_read(&op.") && l.contains("if let Err")) {
                    continue;
                }
                // The refusal block is the handful of lines that follow.
                let end = (i + 9).min(lines.len());
                if lines[i..end]
                    .iter()
                    .take_while(|x| !x.trim_start().starts_with("};"))
                    .any(|x| x.contains("Nfs4Status::BadStateId"))
                {
                    offenders.push(format!("{rel}:{}", i + 1));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "these refuse with a hardcoded BAD_STATEID; use \
             stateids.invalid_status(&stateid) so a revoked delegation \
             gets DELEG_REVOKED: {offenders:?}",
        );
    }

    /// A delegation stateid presented where write rights are required is
    /// an ACCESS-MODE violation, not a bad stateid — RFC 8881 §18.32.3
    /// requires OPENMODE, and before `is_delegation` existed no
    /// access-mode check existed anywhere (the misuse validated
    /// cleanly). WRITE delegations are never granted, so a delegation
    /// entry is read-mode by construction.
    #[test]
    fn a_delegation_stateid_is_flagged_for_openmode() {
        let mgr = StateIdManager::new(crate::state_backend::memory_backend());
        let deleg = mgr.allocate(StateType::Delegation, 1, None);
        assert!(mgr.validate(&deleg).is_ok(), "validates — that is the trap");
        assert!(mgr.is_delegation(&deleg), "the WRITE path's OPENMODE gate");

        let open = mgr.allocate(StateType::Open, 1, None);
        assert!(!mgr.is_delegation(&open), "open stateids must not trip it");
        assert!(!mgr.is_delegation(&super::ANONYMOUS_STATEID));
        assert!(!mgr.is_delegation(&super::READ_BYPASS_STATEID));
    }

    /// RFC 8881 §8.2.2 "current stateid" form: seqid=0 with a known
    /// `other` must pass BOTH validators regardless of the entry's
    /// current seqid (F19 — validate_for_read missed the arm; the
    /// kernel client retried seqid-0 READs in a tight BadStateId loop).
    #[test]
    fn seqid_zero_is_current_for_read_and_write_paths() {
        let mgr = StateIdManager::new(crate::state_backend::memory_backend());
        let stateid = mgr.allocate(StateType::Open, 1, None);
        let bumped = mgr.update_seqid(&stateid).unwrap(); // seqid → 2
        assert_eq!(bumped.seqid, 2);

        let current_form = StateId { seqid: 0, other: stateid.other };
        assert!(mgr.validate(&current_form).is_ok());
        assert!(mgr.validate_for_read(&current_form).is_ok());
    }

    #[test]
    fn test_stateid_validation() {
        let mgr = StateIdManager::new(crate::state_backend::memory_backend());

        let stateid = mgr.allocate(StateType::Open, 1, None);

        // Should be valid
        assert!(mgr.validate(&stateid).is_ok());

        // Wrong seqid should fail
        let mut bad_stateid = stateid;
        bad_stateid.seqid = 99;
        assert!(mgr.validate(&bad_stateid).is_err());

        // Non-existent stateid should fail
        let fake_stateid = StateId {
            seqid: 1,
            other: [0xFF; 12],
        };
        assert!(mgr.validate(&fake_stateid).is_err());
    }

    #[test]
    fn test_seqid_update() {
        let mgr = StateIdManager::new(crate::state_backend::memory_backend());

        let stateid = mgr.allocate(StateType::Open, 1, None);
        assert_eq!(stateid.seqid, 1);

        // Update seqid
        let new_stateid = mgr.update_seqid(&stateid).unwrap();
        assert_eq!(new_stateid.seqid, 2);
        assert_eq!(new_stateid.other, stateid.other);

        // Old seqid should fail validation
        assert!(mgr.validate(&stateid).is_err());

        // New seqid should pass
        assert!(mgr.validate(&new_stateid).is_ok());
    }

    /// RFC 7530 §16.16: a follow-on OPEN by the same (client, owner)
    /// on the same filehandle returns the SAME `stateid.other` with
    /// `seqid` bumped, and merges the share-mask. Pynfs OPEN2
    /// asserts seqid=2 on the second OPEN.
    #[test]
    fn test_record_open_bumps_seqid_for_same_owner() {
        let mgr = StateIdManager::new(crate::state_backend::memory_backend());
        let owner = b"alice".to_vec();
        let fh = b"/path/to/file".to_vec();

        let s1 = mgr.record_open(1, owner.clone(), fh.clone(), 2 /*WRITE*/, 0, None, None);
        assert_eq!(s1.seqid, 1);

        let s2 = mgr.record_open(1, owner.clone(), fh.clone(), 1 /*READ*/, 0, None, None);
        assert_eq!(s2.seqid, 2);
        assert_eq!(s2.other, s1.other, "same owner+fh must reuse stateid.other");

        // The merged share-access should be WRITE | READ = BOTH (3).
        let st = mgr.find_open(1, &owner, &fh).unwrap();
        assert_eq!(st.share_access, 3);

        // validate() agrees about the latest seqid.
        assert!(mgr.validate(&s2).is_ok());
        // The old seqid is stale.
        assert!(mgr.validate(&s1).is_err());
    }

    #[test]
    fn test_record_open_no_seqid_bump_for_identical_masks() {
        let mgr = StateIdManager::new(crate::state_backend::memory_backend());
        let owner = b"fio-worker".to_vec();
        let fh = b"/bench/file0".to_vec();

        let s1 = mgr.record_open(1, owner.clone(), fh.clone(), 2, 0, None, None);
        assert_eq!(s1.seqid, 1);

        // F31: EVERY follow-on OPEN bumps the seqid, even with an
        // unchanged share-mask. (This test previously asserted the
        // opposite — that misreading of §18.16.4 removed the protocol's
        // only defense against reordered same-owner CLOSEs and was the
        // root of the ~7% CLOSE not-found residual / churn collapse.)
        let s2 = mgr.record_open(1, owner.clone(), fh.clone(), 2, 0, None, None);
        assert_eq!(s2.other, s1.other);
        assert_eq!(s2.seqid, 2, "every OPEN must advance the seqid (F31)");

        // seqid=0 "current stateid" form still validates; the stale
        // exact seqid does not.
        assert!(mgr.validate(&StateId { seqid: 0, other: s1.other }).is_ok());
        assert!(mgr.validate(&s1).is_err());
        assert!(mgr.validate(&s2).is_ok());
    }

    #[test]
    fn test_multi_worker_same_owner_fh_seqid_advances_monotonically() {
        let mgr = StateIdManager::new(crate::state_backend::memory_backend());
        let owner = b"linux-open-owner".to_vec();
        let fh = b"/bench/testfile".to_vec();

        let ids: Vec<_> = (0..4)
            .map(|_| mgr.record_open(1, owner.clone(), fh.clone(), 2, 0, None, None))
            .collect();

        for (i, sid) in ids.iter().enumerate() {
            assert_eq!(sid.other, ids[0].other, "same owner+fh → same other");
            assert_eq!(sid.seqid, (i + 1) as u32, "seqid advances per OPEN");
        }
        // Only the latest is exactly valid; older views are stale.
        assert!(mgr.validate(&ids[3]).is_ok());
        assert!(mgr.validate(&ids[0]).is_err());
    }

    /// F31 reproducer: the live failure signature. Backend A's CLOSE is
    /// reordered after backend B's re-OPEN of the same (owner, file).
    /// The stale CLOSE must be refused as OldStateId and must NOT
    /// destroy the state B holds; B's own CLOSE then succeeds.
    #[test]
    fn f31_reordered_stale_close_must_not_destroy_reopened_state() {
        let mgr = StateIdManager::new(crate::state_backend::memory_backend());
        let owner = b"uid-999".to_vec();
        let fh = b"/pgdata/base/16384/1259".to_vec();

        // A opens (seq=1); B re-opens (same mask) → same other, seq=2.
        let a = mgr.record_open(1, owner.clone(), fh.clone(), 3, 0, None, None);
        let b = mgr.record_open(1, owner.clone(), fh.clone(), 3, 0, None, None);
        assert_eq!(a.other, b.other);

        // A's CLOSE arrives late, carrying the stale seq=1 view.
        assert_eq!(mgr.close_open(&a), CloseOutcome::OldStateId);

        // B's state survived: I/O (seqid=0 form) and exact seqid work.
        assert!(mgr.validate(&StateId { seqid: 0, other: b.other }).is_ok());
        assert!(mgr.validate(&b).is_ok());

        // B's CLOSE (current seqid) succeeds; a replay of it is a
        // tombstone hit (benign OldStateId), never BAD_STATEID.
        assert_eq!(mgr.close_open(&b), CloseOutcome::Closed);
        assert_eq!(mgr.close_open(&b), CloseOutcome::OldStateId);

        // Garbage that was never issued IS BAD_STATEID.
        let garbage = StateId { seqid: 1, other: [0xAB; 12] };
        assert_eq!(mgr.close_open(&garbage), CloseOutcome::NotFound);
    }

    /// F31 companion: a late duplicate close of a DEAD stateid must not
    /// tear down the successor open that re-used the same (owner, fh)
    /// key. Exercises the stale-guard index cleanup + remove_if guard.
    #[test]
    fn f31_dead_stateid_close_must_not_kill_successor() {
        let mgr = StateIdManager::new(crate::state_backend::memory_backend());
        let owner = b"uid-999".to_vec();
        let fh = b"/pgdata/pg_wal/0001".to_vec();

        let s1 = mgr.record_open(1, owner.clone(), fh.clone(), 3, 0, None, None);
        // Simulate the crash-artifact path: master revoked, open_states
        // entry left behind — the next OPEN's stale-guard must clean
        // BOTH the entry and its reverse-index record.
        mgr.revoke(&s1).unwrap();
        let s2 = mgr.record_open(1, owner.clone(), fh.clone(), 3, 0, None, None);
        assert_ne!(s1.other, s2.other, "stale guard → fresh allocation");

        // Late duplicate closes of the dead stateid: refused, harmless.
        assert_ne!(mgr.close_open(&s1), CloseOutcome::Closed);
        mgr.close_open_state(&s1.other); // force-close path, same guard

        // The successor is untouched and closes cleanly.
        assert!(mgr.validate(&s2).is_ok());
        assert_eq!(mgr.close_open(&s2), CloseOutcome::Closed);
    }

    /// F31 stress: concurrent open/re-open/close churn on shared
    /// (owner, fh) keys — the pgbench -C pattern. Invariant: a CLOSE of
    /// a stateid the closer was just issued is NEVER NotFound (it is
    /// Closed, or OldStateId when someone re-opened after us), and the
    /// maps fully drain at the end (no leaked entries in any index).
    #[test]
    fn f31_concurrent_open_close_churn_never_not_found() {
        use std::sync::atomic::{AtomicUsize, Ordering as AOrd};
        let mgr = std::sync::Arc::new(StateIdManager::new(
            crate::state_backend::memory_backend(),
        ));
        let not_found = std::sync::Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for t in 0..8 {
            let mgr = mgr.clone();
            let not_found = not_found.clone();
            handles.push(std::thread::spawn(move || {
                let owner = b"uid-999".to_vec();
                for i in 0..500 {
                    let fh = format!("/pgdata/rel{}", (t + i) % 4).into_bytes();
                    let sid = mgr.record_open(1, owner.clone(), fh, 3, 0, None, None);
                    if mgr.close_open(&sid) == CloseOutcome::NotFound {
                        not_found.fetch_add(1, AOrd::SeqCst);
                    }
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(
            not_found.load(AOrd::SeqCst),
            0,
            "a just-issued stateid must never be BAD_STATEID at close (F31)"
        );

        // Drain any opens that lost their close to an OldStateId race.
        let leftover: Vec<[u8; 12]> = mgr
            .open_state_keys
            .iter()
            .map(|e| *e.key())
            .collect();
        for other in leftover {
            mgr.close_open_state(&other);
        }
        assert!(mgr.open_states.is_empty(), "open_states must drain");
        assert!(mgr.open_state_keys.is_empty(), "reverse index must drain");
        assert!(mgr.opens_by_fh.is_empty(), "by-fh index must drain");
    }

    #[test]
    fn test_close_then_reopen_gets_fresh_stateid() {
        let mgr = StateIdManager::new(crate::state_backend::memory_backend());
        let owner = b"linux-owner".to_vec();
        let fh = b"/tmp/testfile".to_vec();

        let sid1 = mgr.record_open(1, owner.clone(), fh.clone(), 3, 0, None, None);
        assert!(mgr.validate(&sid1).is_ok());

        mgr.close_open_state(&sid1.other);
        assert!(mgr.validate(&sid1).is_err());

        let sid2 = mgr.record_open(1, owner.clone(), fh.clone(), 3, 0, None, None);
        assert_ne!(sid1.other, sid2.other);
        assert!(mgr.validate(&sid2).is_ok());
    }

    #[test]
    fn test_share_conflict_detection() {
        let mgr = StateIdManager::new(crate::state_backend::memory_backend());
        let fh = b"/path/to/file".to_vec();

        // Owner A opens with deny=WRITE.
        mgr.record_open(1, b"alice".to_vec(), fh.clone(), 1, 2, None, None);

        // Different (client, owner) trying to access WRITE — conflicts.
        assert!(mgr.share_conflict(&fh, 2, b"bob", 2 /*WRITE*/, 0));
        // Different owner accessing READ only — no conflict (deny mask
        // only covers WRITE).
        assert!(!mgr.share_conflict(&fh, 2, b"bob", 1 /*READ*/, 0));
        // Same (client, owner) is always exempt.
        assert!(!mgr.share_conflict(&fh, 1, b"alice", 2 /*WRITE*/, 0));

        // Symmetric conflict: a new owner deny=READ vs existing
        // access=READ is also a conflict.
        assert!(mgr.share_conflict(&fh, 2, b"bob", 0, 1 /*deny READ*/));
    }

    /// RFC 8881 §18.16.5: EXCLUSIVE4 retry with a matching verifier
    /// finds the existing open (idempotent retry). Different
    /// verifier on existing file → caller maps to EXIST.
    #[test]
    fn test_find_exclusive_match() {
        let mgr = StateIdManager::new(crate::state_backend::memory_backend());
        let fh = b"/excl/file".to_vec();

        mgr.record_open(1, b"alice".to_vec(), fh.clone(), 3, 0, Some(0xdead_beef), None);

        // Same verifier → matches.
        assert!(mgr.find_exclusive_match(&fh, 0xdead_beef).is_some());
        // Different verifier → no match (caller returns EXIST).
        assert!(mgr.find_exclusive_match(&fh, 0xc0ffee).is_none());
        // No prior exclusive open on a different fh.
        assert!(mgr.find_exclusive_match(b"/other", 0xdead_beef).is_none());
    }

    #[test]
    fn test_stateid_revocation() {
        let mgr = StateIdManager::new(crate::state_backend::memory_backend());

        let stateid = mgr.allocate(StateType::Open, 1, None);
        assert!(mgr.validate(&stateid).is_ok());

        // Revoke
        mgr.revoke(&stateid).unwrap();

        // Should fail validation
        assert!(mgr.validate(&stateid).is_err());

        // Should still be in active count
        assert_eq!(mgr.active_count(), 1);
    }

    #[test]
    fn test_stateid_removal() {
        let mgr = StateIdManager::new(crate::state_backend::memory_backend());

        let stateid = mgr.allocate(StateType::Open, 1, None);
        assert_eq!(mgr.active_count(), 1);

        mgr.remove(&stateid);

        assert_eq!(mgr.active_count(), 0);
        assert!(mgr.validate(&stateid).is_err());
    }

    #[test]
    fn test_client_stateids() {
        let mgr = StateIdManager::new(crate::state_backend::memory_backend());

        // Client 1 has 2 stateids
        let _s1 = mgr.allocate(StateType::Open, 1, None);
        let _s2 = mgr.allocate(StateType::Lock, 1, None);

        // Client 2 has 1 stateid
        let _s3 = mgr.allocate(StateType::Open, 2, None);

        let client1_states = mgr.get_client_stateids(1);
        let client2_states = mgr.get_client_stateids(2);

        assert_eq!(client1_states.len(), 2);
        assert_eq!(client2_states.len(), 1);
    }

    #[test]
    fn test_client_cleanup() {
        let mgr = StateIdManager::new(crate::state_backend::memory_backend());

        mgr.allocate(StateType::Open, 1, None);
        mgr.allocate(StateType::Lock, 1, None);
        mgr.allocate(StateType::Open, 2, None);

        assert_eq!(mgr.active_count(), 3);

        // Remove all client 1 stateids
        mgr.remove_client_stateids(1);

        assert_eq!(mgr.active_count(), 1);
        assert_eq!(mgr.get_client_stateids(1).len(), 0);
        assert_eq!(mgr.get_client_stateids(2).len(), 1);
    }

    #[test]
    fn test_count_by_type() {
        let mgr = StateIdManager::new(crate::state_backend::memory_backend());

        mgr.allocate(StateType::Open, 1, None);
        mgr.allocate(StateType::Open, 2, None);
        mgr.allocate(StateType::Lock, 1, None);

        assert_eq!(mgr.count_by_type(StateType::Open), 2);
        assert_eq!(mgr.count_by_type(StateType::Lock), 1);
        assert_eq!(mgr.count_by_type(StateType::Delegation), 0);
    }

    #[test]
    fn test_special_stateids() {
        let mgr = StateIdManager::new(crate::state_backend::memory_backend());

        // Anonymous stateid should validate
        assert!(mgr.validate(&ANONYMOUS_STATEID).is_ok());

        // Read bypass stateid should validate
        assert!(mgr.validate(&READ_BYPASS_STATEID).is_ok());
    }

    #[test]
    fn open_downgrade_shrinks_masks_and_bumps_seqid() {
        let mgr = StateIdManager::new(crate::state_backend::memory_backend());
        // BOTH access (3), deny NONE.
        let sid = mgr.record_open(1, b"owner".to_vec(), b"fh".to_vec(), 3, 0, None, None);

        let refreshed = mgr
            .downgrade_open(&sid, 1 /* READ only */, 0)
            .expect("subset downgrade must succeed");
        assert_eq!(refreshed.other, sid.other, "same open, refreshed seqid");
        assert_eq!(refreshed.seqid, sid.seqid.wrapping_add(1));

        let state = mgr.find_open(1, b"owner", b"fh").unwrap();
        assert_eq!(state.share_access, 1);
        assert_eq!(state.share_deny, 0);
    }

    #[test]
    fn open_downgrade_refuses_upgrades_and_unknown_stateids() {
        use crate::nfs::v4::protocol::Nfs4Status;
        let mgr = StateIdManager::new(crate::state_backend::memory_backend());
        // READ only.
        let sid = mgr.record_open(1, b"owner".to_vec(), b"fh".to_vec(), 1, 0, None, None);

        // Asking for WRITE (2) is an upgrade → INVAL.
        assert_eq!(mgr.downgrade_open(&sid, 2, 0), Err(Nfs4Status::Inval));
        // Empty access is meaningless → INVAL.
        assert_eq!(mgr.downgrade_open(&sid, 0, 0), Err(Nfs4Status::Inval));
        // Unknown stateid → BAD_STATEID.
        let bogus = StateId { seqid: 1, other: [9u8; 12] };
        assert_eq!(mgr.downgrade_open(&bogus, 1, 0), Err(Nfs4Status::BadStateId));
    }

    #[test]
    fn write_open_is_visible_by_inode_and_gone_on_close() {
        let mgr = StateIdManager::new(crate::state_backend::memory_backend());
        let ident = Some((7, 700));
        // Read-only open: no write visible.
        let s1 = mgr.record_open(1, b"a".to_vec(), b"fh1".to_vec(), 1, 0, None, ident);
        assert!(!mgr.file_has_write_open(7, 700));
        // A second client opens WRITE on the same inode.
        let s2 = mgr.record_open(2, b"b".to_vec(), b"fh1".to_vec(), 2, 0, None, ident);
        assert!(mgr.file_has_write_open(7, 700));
        // Different inode untouched.
        assert!(!mgr.file_has_write_open(7, 701));
        // Writer closes: predicate clears. Reader closes: index drains.
        assert_eq!(mgr.close_open(&s2), CloseOutcome::Closed);
        assert!(!mgr.file_has_write_open(7, 700));
        assert_eq!(mgr.close_open(&s1), CloseOutcome::Closed);
        assert!(!mgr.file_has_write_open(7, 700));
    }

    #[test]
    fn a_hardlink_alias_write_is_seen_across_fhs() {
        // One inode, two fhs (path-handles mode hardlinks). A write
        // open through EITHER name must be visible to the predicate —
        // the invariant fh-keying would have falsified (design §4
        // rule 5).
        let mgr = StateIdManager::new(crate::state_backend::memory_backend());
        let ident = Some((7, 700));
        mgr.record_open(1, b"a".to_vec(), b"name-a".to_vec(), 1, 0, None, ident);
        let w = mgr.record_open(2, b"b".to_vec(), b"name-b".to_vec(), 2, 0, None, ident);
        assert!(mgr.file_has_write_open(7, 700));
        assert_eq!(mgr.close_open(&w), CloseOutcome::Closed);
        assert!(!mgr.file_has_write_open(7, 700));
    }

    #[test]
    fn an_unidentified_open_is_invisible_to_the_predicate() {
        // ident None (stat failed at OPEN): the open is not indexed,
        // the predicate answers false, and the only consequence is
        // that grants on the file are refused elsewhere — never a
        // false "no writers" that a grant could trust... which is why
        // the grant path must ALSO refuse when it cannot resolve the
        // opened file's own identity (design §4: refusal is free).
        let mgr = StateIdManager::new(crate::state_backend::memory_backend());
        mgr.record_open(1, b"a".to_vec(), b"fh1".to_vec(), 2, 0, None, None);
        assert!(!mgr.file_has_write_open(7, 700));
    }

    #[test]
    fn client_teardown_drains_the_identity_index() {
        let mgr = StateIdManager::new(crate::state_backend::memory_backend());
        mgr.record_open(1, b"a".to_vec(), b"fh1".to_vec(), 2, 0, None, Some((7, 700)));
        assert!(mgr.file_has_write_open(7, 700));
        mgr.remove_client_stateids(1);
        assert!(!mgr.file_has_write_open(7, 700));
    }
}
