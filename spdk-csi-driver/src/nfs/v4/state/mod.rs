// NFSv4 State Management
//
// NFSv4 is a stateful protocol (unlike NFSv3). The server maintains:
// - Client IDs (established via EXCHANGE_ID)
// - Sessions (established via CREATE_SESSION)
// - StateIds (for opens, locks, delegations)
// - Leases (clients must renew or lose state)
//
// State Lifecycle:
// 1. Client connects → EXCHANGE_ID → receives clientid
// 2. Client establishes session → CREATE_SESSION → receives sessionid
// 3. Client performs operations → SEQUENCE (in every COMPOUND) → renews lease
// 4. Client opens file → OPEN → receives stateid
// 5. Client doesn't renew → lease expires → state cleanup
//
// This module implements all state tracking for NFSv4.1/4.2

pub mod client;
pub mod session;
pub mod stateid;
pub mod lease;
pub mod delegation;
mod sched_map;
pub mod deleg_meter;
#[cfg(test)]
mod deleg_stress_tests;

#[cfg(all(test, feature = "shuttle-test"))]
mod deleg_shuttle_tests;

pub use client::ClientManager;
pub use session::SessionManager;
pub use stateid::{CloseOutcome, StateIdManager, StateType, StateEntry};
pub use lease::LeaseManager;
pub use delegation::{
    deleg_flag_exclusive, delegations_enabled, override_delegations_enabled,
    override_pnfs_delegations_enabled, pnfs_delegations_enabled, with_delegations,
    with_delegations_pnfs, DelegFlagGuard, DelegReturnError, DelegSnapshot,
    DelegState, DelegationManager, FenceOutcome, FenceVerdict, FileId, GrantRefusal,
    MutationGuard, RecallOrder,
};

use crate::nfs::v4::protocol::SessionId;
use crate::state_backend::{StateBackend, StateBackendError};
use std::sync::Arc;

/// Quotas on the server-side NFS state tables. Every table below is
/// minted by unauthenticated wire ops (AUTH_SYS verifies nothing), each
/// entry costs memory AND a persisted state.db row on the PVC, and none
/// had any bound — one TCP-reachable peer could grow them until the hub
/// OOMed or the volume filled. Refusals are NFS4ERR_DELAY, the knfsd
/// precedent: retryable, and the courtesy sweep frees capacity from
/// clients that stopped renewing, so a legitimate burst that hits a cap
/// succeeds on retry. The caps are quotas, not invariants — a concurrent
/// race may briefly over-admit by a few entries, which is fine.
#[derive(Debug, Clone, Copy)]
pub struct StateQuotas {
    /// Global cap on client records, confirmed + unconfirmed
    /// (`FLINT_NFS_MAX_CLIENTS`, default 4096). The load-bearing one:
    /// minting a fresh client identity is the cheapest unauthenticated
    /// growth vector, and every record also carries a lease the sweep
    /// must walk.
    pub max_clients: usize,
    /// Sessions per client (`FLINT_NFS_MAX_SESSIONS_PER_CLIENT`,
    /// default 16; Linux uses 1). Each session owns a slot table whose
    /// reply cache is a standing grant — without this cap, one client
    /// multiplies the per-session bound without limit.
    pub max_sessions_per_client: usize,
    /// Stateids per client (`FLINT_NFS_MAX_STATEIDS_PER_CLIENT`,
    /// default 65536) — bounds OPEN/LOCK state and, transitively, the
    /// persisted lock-owner registrations.
    pub max_stateids_per_client: usize,
    /// Byte-range lock entries per client
    /// (`FLINT_NFS_MAX_LOCKS_PER_CLIENT`, default 65536).
    pub max_locks_per_client: usize,
}

impl StateQuotas {
    pub fn from_env() -> Self {
        fn env_or(name: &str, default: usize) -> usize {
            std::env::var(name)
                .ok()
                .and_then(|v| v.parse().ok())
                .filter(|&n| n > 0)
                .unwrap_or(default)
        }
        Self {
            max_clients: env_or("FLINT_NFS_MAX_CLIENTS", 4096),
            max_sessions_per_client: env_or("FLINT_NFS_MAX_SESSIONS_PER_CLIENT", 16),
            max_stateids_per_client: env_or("FLINT_NFS_MAX_STATEIDS_PER_CLIENT", 65536),
            max_locks_per_client: env_or("FLINT_NFS_MAX_LOCKS_PER_CLIENT", 65536),
        }
    }
}

/// NFSv4 state manager - coordinates all state components
pub struct StateManager {
    pub clients: Arc<ClientManager>,
    pub sessions: Arc<SessionManager>,
    pub stateids: Arc<StateIdManager>,
    pub leases: Arc<LeaseManager>,
    pub delegations: Arc<DelegationManager>,
    /// See [`StateQuotas`]. Read by the EXCHANGE_ID / CREATE_SESSION /
    /// OPEN / LOCK handlers at their mint points.
    pub quotas: StateQuotas,
    /// Shared persistence target. Each per-component manager holds its
    /// own clone; this field exists so `load_from_backend` and
    /// post-startup helpers can reach the trait without going through
    /// a sub-manager.
    backend: Arc<dyn StateBackend>,
    /// Per-client pending SEQ4_STATUS_* bits, OR'd into every SEQUENCE
    /// reply for that client (RFC 8881 §18.46.3 — the reply's
    /// sr_status_flags used to be hardcoded 0, which is how a lease
    /// sweep once stripped a client's locks without the client ever
    /// being told). LEVEL-triggered, not edge: a bit stays up until the
    /// condition clears (e.g. RECALLABLE_STATE_REVOKED until the last
    /// revoked delegation is FREE_STATEID'd), so readers never consume
    /// it. In-memory only — restart re-arming comes from the persisted
    /// holder-evidence markers, not from this map.
    /// (docs/plans/nfs-delegations-design.md §5.4/§6, slice 2.)
    seq_flags: dashmap::DashMap<u64, u32>,
    /// How a fence conflict's CB_RECALLs get sent — installed at
    /// server bring-up once a RecallDriver exists (the driver needs a
    /// CallbackManager, which needs the listener's back-channel
    /// registry, so it cannot exist at StateManager construction).
    /// A closure rather than the driver type keeps `state` from
    /// depending on `deleg_recall` (which depends back on `state`).
    recall_spawner: std::sync::OnceLock<Arc<dyn Fn(Vec<RecallOrder>) + Send + Sync>>,
    /// The listener's per-session back-channel writer registry,
    /// installed by the dispatcher at construction. `callback_ready`
    /// (delegation grant rule 7) reads it; before installation no
    /// client is callback-ready, so no delegation can be granted.
    back_channels: std::sync::OnceLock<
        Arc<dashmap::DashMap<SessionId, Vec<Arc<crate::nfs::v4::back_channel::BackChannelWriter>>>>,
    >,
    /// Set when this server runs the MDS role. Grants in that posture
    /// additionally require FLINT_NFS_DELEGATIONS_PNFS (design §3,
    /// slice 5) and pass grant rule 6 through `layout_probe` below —
    /// granting without the write-capable-layout check would hand out
    /// delegations that pNFS writers silently invalidate.
    pnfs_posture: std::sync::OnceLock<()>,
    /// Grant rule 6's oracle (design §4): the clients holding a
    /// write-capable (RW or ANY) layout on the file at `file_key`
    /// (export-relative path). Installed by the dispatcher alongside
    /// `set_pnfs_posture`, from the pNFS handler's layout index. In
    /// MDS posture with NO probe installed the rule answers "held by
    /// someone else" — fail closed — because an MDS that grants
    /// without consulting its layouts is the silent-stale case.
    layout_probe: std::sync::OnceLock<Arc<dyn Fn(&str) -> Vec<u64> + Send + Sync>>,
    /// Clients whose persisted holder-evidence marker was found at
    /// load (design §6): their SEQ4 RECALLABLE_STATE_REVOKED bit is
    /// pre-armed, and the value tracks delivery — None until a
    /// SEQUENCE reply has carried the bit, then the (session, slot,
    /// seq) it rode on. A later SEQUENCE advancing that same slot is
    /// the RFC's own acknowledgment that the reply was received
    /// (§2.10.6.1), and only then is the marker consumed. This is the
    /// model's RenewConsume: free the evidence only when the signal
    /// provably arrived.
    armed_markers: dashmap::DashMap<u64, Option<(SessionId, u32, u32)>>,
}

/// The synthetic `other` of a client's holder-evidence marker row:
/// a 0xFD magic prefix (a counter would need ~2^32 boots of mints to
/// collide) + the client id.
fn marker_other(client_id: u64) -> [u8; 12] {
    let mut o = [0xFDu8; 12];
    o[4..12].copy_from_slice(&client_id.to_be_bytes());
    o
}

/// The single row that carries a persisted breaker trip. Tagged
/// distinctly from holder markers (0xFD) so the loader can tell them
/// apart, and keyed on a CONSTANT so a second trip overwrites the
/// first rather than accumulating rows nothing ever deletes.
fn breaker_marker_other() -> [u8; 12] {
    [stateid::BREAKER_MARKER_TAG; 12]
}

impl StateManager {
    /// Create a new state manager backed by `backend`. Use
    /// `state_backend::memory_backend()` for tests / dev work, or a
    /// `SqliteBackend` for production.
    pub fn new(volume_id: &str, backend: Arc<dyn StateBackend>) -> Self {
        let lease_manager = Arc::new(LeaseManager::new());
        let client_manager = Arc::new(ClientManager::new(
            lease_manager.clone(),
            volume_id,
            Arc::clone(&backend),
        ));
        let session_manager = Arc::new(SessionManager::new(Arc::clone(&backend)));
        let stateid_manager = Arc::new(StateIdManager::new(Arc::clone(&backend)));
        let delegation_manager = Arc::new(DelegationManager::new());

        // Holder evidence (design §6): every transition in whether a
        // client holds recallable state Puts or Deletes its marker
        // row — a Delegation-typed StateIdRecord with a synthetic
        // key. The backend queue coalesces by key, so per-transition
        // writes cost one queued row at most.
        {
            let b = Arc::clone(&backend);
            delegation_manager.install_evidence(Arc::new(move |client_id, holds| {
                if holds {
                    b.enqueue_write(crate::state_backend::WriteOp::PutStateid(
                        crate::state_backend::StateIdRecord {
                            other: marker_other(client_id),
                            seqid: 0,
                            state_type: crate::state_backend::StateTypeRecord::Delegation,
                            client_id,
                            filehandle: None,
                            revoked: true,
                        },
                    ));
                } else {
                    b.enqueue_write(crate::state_backend::WriteOp::DeleteStateid(
                        marker_other(client_id),
                    ));
                }
            }));

            let lm = Arc::clone(&lease_manager);
            delegation_manager.install_lease_probe(Arc::new(move |client_id| {
                // Expired means "there IS a lease row and it has
                // lapsed" — NOT `!is_valid`, which also answers false
                // for a client with no row at all.
                //
                // An absent row is ambiguous: it can mean the lease was
                // swept (the client really is gone) or that this code
                // path never created one. Absence is not evidence, and
                // the safe reading for a REVOCATION decision is "do not
                // short-circuit" — the ladder will discover an
                // unreachable client on its own and take the
                // path_down route, which is slower and correct.
                lm.get_remaining(client_id)
                    .map(|d| d.is_zero())
                    .unwrap_or(false)
            }));

            let b2 = Arc::clone(&backend);
            delegation_manager.install_breaker_sink(Arc::new(move |trip: Option<u64>| {
                match trip {
                    Some(unix) => {
                        let mut other = breaker_marker_other();
                        other[4..12].copy_from_slice(&unix.to_be_bytes());
                        b2.enqueue_write(crate::state_backend::WriteOp::PutStateid(
                            crate::state_backend::StateIdRecord {
                                other,
                                seqid: 0,
                                state_type: crate::state_backend::StateTypeRecord::Delegation,
                                // Not a client's row. Zero here is the
                                // absence of a client, and the loader
                                // never reads it as one because the tag
                                // byte routes this row elsewhere.
                                client_id: 0,
                                filehandle: None,
                                revoked: true,
                            },
                        ));
                    }
                    None => {
                        b2.enqueue_write(crate::state_backend::WriteOp::DeleteStateid(
                            breaker_marker_other(),
                        ));
                    }
                }
            }));
        }

        Self {
            clients: client_manager,
            sessions: session_manager,
            stateids: stateid_manager,
            leases: lease_manager,
            delegations: delegation_manager,
            quotas: StateQuotas::from_env(),
            backend,
            seq_flags: dashmap::DashMap::new(),
            recall_spawner: std::sync::OnceLock::new(),
            back_channels: std::sync::OnceLock::new(),
            pnfs_posture: std::sync::OnceLock::new(),
            layout_probe: std::sync::OnceLock::new(),
            armed_markers: dashmap::DashMap::new(),
        }
    }

    /// Delivery bookkeeping for a load-armed holder-evidence marker
    /// (design §6), called from the SEQUENCE arm for every NEW (non-
    /// replay) request. First call records the reply that carries the
    /// bit; a later new SEQUENCE advancing the SAME session+slot
    /// proves that reply was received (RFC 8881 §2.10.6.1 slot-ack) —
    /// the marker is consumed: row deleted, bit lowered unless real
    /// tombstones (or a fresh restart's marker) still hold it up.
    pub fn note_seq4_delivery(
        &self,
        client_id: u64,
        session_id: SessionId,
        slot: u32,
        seq: u32,
    ) {
        let Some(mut armed) = self.armed_markers.get_mut(&client_id) else {
            return;
        };
        match *armed {
            None => {
                *armed = Some((session_id, slot, seq));
            }
            Some((s, sl, sq)) if s == session_id && sl == slot && seq > sq => {
                drop(armed);
                self.armed_markers.remove(&client_id);
                // The marker's job is done — unless the client
                // meanwhile acquired NEW recallable state, in which
                // case the evidence sink has already re-Put the row
                // and deleting it here would erase live evidence.
                if !self.delegations.client_holds_live(client_id)
                    && !self.delegations.client_has_revoked(client_id)
                {
                    self.backend
                        .enqueue_write(crate::state_backend::WriteOp::DeleteStateid(
                            marker_other(client_id),
                        ));
                    self.lower_seq_flags(
                        client_id,
                        crate::nfs::v4::protocol::seq4_status::RECALLABLE_STATE_REVOKED,
                    );
                }
            }
            Some(_) => {}
        }
    }

    /// Is the client's load-armed marker still awaiting consumption?
    /// (Rigs and the idle input read this.)
    pub fn marker_armed(&self, client_id: u64) -> bool {
        self.armed_markers.contains_key(&client_id)
    }

    /// Mark this server as running the MDS role (dispatcher, once).
    pub fn set_pnfs_posture(&self) {
        let _ = self.pnfs_posture.set(());
    }

    pub fn pnfs_posture(&self) -> bool {
        self.pnfs_posture.get().is_some()
    }

    /// Install grant rule 6's layout oracle (dispatcher, once, whenever
    /// it sets the MDS posture). `probe(file_key)` answers the client
    /// ids holding a write-capable layout on that file.
    pub fn install_layout_probe(
        &self,
        probe: Arc<dyn Fn(&str) -> Vec<u64> + Send + Sync>,
    ) {
        let _ = self.layout_probe.set(probe);
    }

    /// Whether rule 6 has its oracle. A constructor test pins that
    /// every MDS-posture dispatcher installs one: without it the rule
    /// fails closed and every grant is refused — silently, like the
    /// missing recall machinery was.
    pub fn layout_probe_installed(&self) -> bool {
        self.layout_probe.get().is_some()
    }

    /// Grant rule 6 (design §4): does a client OTHER than `requester`
    /// hold a write-capable (RW or ANY) layout on `file_key`? Always
    /// false outside the MDS posture (no layouts exist). Inside it,
    /// consulted through the layout index the pNFS handler maintains
    /// — that index is keyed by the placement's immutable file
    /// identity, so it follows the file through RENAME; the design's
    /// concern about a path-keyed layout table applies only to legacy
    /// `path:` pins, which cannot be renamed at all (the op is
    /// refused). No probe in MDS posture ⇒ true (refuse).
    pub fn write_layout_held_by_other(&self, file_key: &str, requester: u64) -> bool {
        if !self.pnfs_posture() {
            return false;
        }
        match self.layout_probe.get() {
            Some(probe) => probe(file_key).into_iter().any(|c| c != requester),
            None => true,
        }
    }

    /// Install the back-channel writer registry (dispatcher, once).
    pub fn install_back_channels(
        &self,
        registry: Arc<
            dashmap::DashMap<
                SessionId,
                Vec<Arc<crate::nfs::v4::back_channel::BackChannelWriter>>,
            >,
        >,
    ) {
        let _ = self.back_channels.set(registry);
    }

    /// Is `client_id` reachable for callbacks — delegation grant rule
    /// 7 (design §4), and the predicate `handle_layoutget` has always
    /// lacked. Four clauses per session, first ready session wins:
    /// cb_program advertised, an emittable cb_cred (AUTH_SYS/NONE —
    /// GSS callbacks are recognised-but-unemittable, so a GSS-only
    /// client would fail every recall), back-channel
    /// ca_maxoperations >= 2 (CB_SEQUENCE+CB_RECALL is a 2-op
    /// compound), and a live bound writer. No CB_NULL probe: in v4.1
    /// the bound session back-channel is the RFC-sanctioned
    /// verification; the recall ladder handles a channel that later
    /// lies.
    pub fn callback_ready(&self, client_id: u64) -> bool {
        let Some(back_channels) = self.back_channels.get() else {
            return false;
        };
        for sid in self.sessions.get_client_sessions(client_id) {
            let Some(sess) = self.sessions.get_session(&sid) else {
                continue;
            };
            if sess.cb_program == 0 {
                continue;
            }
            if matches!(
                sess.cb_cred,
                Some(crate::nfs::v4::compound::CallbackSecParms::Gss)
            ) {
                continue;
            }
            if sess.back_chan_maxops < 2 {
                continue;
            }
            let has_writer = back_channels
                .get(&sid)
                .map(|w| !w.is_empty())
                .unwrap_or(false);
            if has_writer {
                return true;
            }
        }
        false
    }

    /// Install the recall spawner (server bring-up, once). Grants are
    /// refused until this exists — a delegation the server cannot
    /// recall is the stale-forever trap.
    pub fn install_recall_spawner(
        &self,
        spawner: Arc<dyn Fn(Vec<RecallOrder>) + Send + Sync>,
    ) {
        let _ = self.recall_spawner.set(spawner);
    }

    /// Can the grant path run at all? (Design §4 precondition: no
    /// recall machinery, no grants.)
    pub fn recall_machinery_ready(&self) -> bool {
        self.recall_spawner.get().is_some()
    }

    /// THE fence funnel (design §5.2) — every mutation lane consults
    /// here pre-op. `Proceed(guard)` means run the mutation and hold
    /// the guard until it completes; `Delay` means answer
    /// NFS4ERR_DELAY (the recalls are already on their way). With the
    /// feature off — or before the spawner is installed, when no
    /// grant can have happened — this is one atomic load.
    /// `site` names the §5.2 conflict site for
    /// `delay_answered_total{site}`. It is a caller-supplied label
    /// because only the caller knows which operation it is — deriving
    /// it here would need a guess, and a mislabelled delay is worse
    /// than none: the conflict-site matrix in §9 asserts per-site
    /// equalities, so a wrong label turns a real regression into a
    /// green leg somewhere else.
    pub fn deleg_fence(
        &self,
        ident: (u64, u64),
        mutator: Option<u64>,
        truncate: bool,
        site: &'static str,
    ) -> FenceVerdict {
        if !delegation::delegations_enabled() {
            return FenceVerdict::Proceed(None);
        }
        let Some(spawn) = self.recall_spawner.get() else {
            return FenceVerdict::Proceed(None);
        };
        match self
            .delegations
            .mutation_fence(FileId::new(ident.0, ident.1), mutator, truncate)
        {
            FenceOutcome::Clear(g) => FenceVerdict::Proceed(Some(g)),
            FenceOutcome::Conflict {
                mut guard,
                recalls,
                expired,
                delay,
            } => {
                // Expired-courtesy holders: revoke here, outside the
                // entry lock, instead of spending the 90s ladder on a
                // client whose lease already lapsed. Loud, never
                // silent — the tombstone and the SEQ4 bit are what
                // make the lease-check race safe rather than merely
                // unlikely.
                let had_expired = !expired.is_empty();
                for stateid in expired {
                    if let Some(client) = self.delegations.revoke(&stateid) {
                        let _ = self.stateids.revoke(&stateid);
                        self.raise_seq_flags(
                            client,
                            crate::nfs::v4::protocol::seq4_status::RECALLABLE_STATE_REVOKED,
                        );
                        self.delegations
                            .meter()
                            .note_revoked(deleg_meter::RevokeReason::LeaseExpired);
                        tracing::warn!(
                            "deleg: client {} held a delegation on an EXPIRED lease — \
                             revoked without a recall (it cannot answer one)",
                            client
                        );
                    }
                }
                if !recalls.is_empty() {
                    spawn(recalls);
                }
                if delay {
                    // The conflictor gives up this attempt; its guard
                    // drops with the verdict.
                    drop(guard);
                    self.delegations.meter().note_delay(site);
                    FenceVerdict::Delay
                } else {
                    // The expired holders are tombstones now, so the
                    // release-time exclusivity check can be armed
                    // (the funnel left it unarmed for exactly this
                    // arm — see `mutation_fence`).
                    if had_expired {
                        guard.arm_exclusivity_check();
                    }
                    FenceVerdict::Proceed(Some(guard))
                }
            }
        }
    }

    /// Start the delegation reporter (design §10). One line per
    /// interval, INFO so it survives a server running at default
    /// level — four discriminators in the F68 hunt were vacuous
    /// because the evidence was `debug!`-only.
    ///
    /// **When the gate is ON the line is printed every interval even
    /// if every counter is zero.** That is deliberate and is the
    /// opposite of the f68a meter's silence-when-idle. §9's gate-off
    /// vacuity leg asserts `deleg_granted_total == 0` under the full
    /// rig, and a rig cannot tell "zero grants" from "no reporter
    /// running" if zero is expressed as silence. A printed zero is
    /// evidence; an inferred zero is an absence, and this codebase has
    /// paid for that difference more than once.
    ///
    /// When the gate is OFF the feature is dark and there is nothing
    /// to meter, so the reporter says so once and exits rather than
    /// printing zeros forever.
    ///
    /// `FLINT_NFS_DELEG_REPORT_SECS` (default 60) tunes the interval.
    pub fn start_deleg_reporter(self: &std::sync::Arc<Self>) {
        let interval = std::env::var("FLINT_NFS_DELEG_REPORT_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|s| *s > 0)
            .unwrap_or(60);
        if !delegation::delegations_enabled() {
            tracing::info!(
                "📊 deleg reporter: delegations are OFF (FLINT_NFS_DELEGATIONS unset) — \
                 no grants will be made and no periodic report follows"
            );
            return;
        }
        // The server's OWN word for the posture it is in and the gates
        // that apply to it. Without this line a rig (and an operator)
        // can only infer from silence why an MDS granted nothing —
        // and "refused every grant" is indistinguishable from "no
        // workload qualified", which is exactly how this feature
        // shipped inert once. Three things decide whether an MDS ever
        // grants, so all three are stated.
        let posture = if self.pnfs_posture() { "MDS" } else { "standalone" };
        let gate = if !self.pnfs_posture() {
            "n/a (not an MDS)".to_string()
        } else if delegation::pnfs_delegations_enabled() {
            "ON".to_string()
        } else {
            "OFF (FLINT_NFS_DELEGATIONS_PNFS unset) — every grant refused in this posture"
                .to_string()
        };
        let probe = if !self.pnfs_posture() {
            "n/a"
        } else if self.layout_probe_installed() {
            "installed"
        } else {
            "MISSING — grant rule 6 fails CLOSED, so every grant is refused"
        };
        tracing::info!(
            "📊 deleg reporter: delegations are ON · posture={} · pnfs gate={} · layout probe={}",
            posture,
            gate,
            probe
        );
        let sm = std::sync::Arc::clone(self);
        tokio::spawn(async move {
            let mut prev = sm.delegations.totals();
            let mut tick =
                tokio::time::interval(std::time::Duration::from_secs(interval));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tick.tick().await; // fires immediately; skip so the first report is a full interval
            loop {
                tick.tick().await;
                let cur = sm.delegations.totals();
                let delta = cur.delta(&prev);
                prev = cur;
                let meter = sm.delegations.meter();
                tracing::info!(
                    "📊 deleg last {}s: {}",
                    interval,
                    delta.render(
                        sm.delegations.live_count(),
                        sm.delegations.files_under_recall(),
                        meter.latency_percentile_ms(0.99),
                    )
                );
            }
        });
    }

    /// Raise SEQ4_STATUS_* bits for a client. Every subsequent SEQUENCE
    /// from that client carries them until `lower_seq_flags` clears them.
    pub fn raise_seq_flags(&self, client_id: u64, bits: u32) {
        if bits == 0 {
            return;
        }
        let mut e = self.seq_flags.entry(client_id).or_insert(0);
        // Count the TRANSITION, not the call. These flags are
        // level-triggered, so a re-raise of an already-set bit is a
        // no-op on the wire; counting it would inflate
        // seq4_flag_raised_total every time the ladder retried and
        // make a rig's "== 1" assertion unwritable.
        let newly = bits & !*e;
        *e |= bits;
        drop(e);
        if newly != 0 {
            self.delegations.meter().note_seq4(newly);
        }
    }

    /// A back-channel writer was NEWLY bound for `session_id` — a
    /// CREATE_SESSION whose reply echoed CONN_BACK_CHAN, or a
    /// BIND_CONN_TO_SESSION with dir BACK/BOTH. Two things follow, and
    /// design §5.4 ("rearm-on-rebind") wants both:
    ///
    /// 1. **CB_PATH_DOWN comes down.** The bit is level-triggered, and
    ///    until this existed the only site that lowered it was a
    ///    recall that later succeeded. A client whose channel died,
    ///    whose recall then revoked, and whose ladder exited therefore
    ///    carried the bit on EVERY SEQUENCE reply for the rest of its
    ///    life — asserting a broken path long after it was repaired.
    ///    RFC 8881 §2.10.4 makes BIND_CONN_TO_SESSION the client's
    ///    prescribed response to the bit, so a stuck bit does not just
    ///    misinform: it drives the repair operation in a loop, and the
    ///    loop's every iteration lands right back here.
    /// 2. **Parked ladders re-drive at once** instead of up to one
    ///    `path_retry` later — the difference between a reconnect
    ///    completing a recall and a conflicting writer eating another
    ///    round of DELAY.
    ///
    /// Called only on a genuinely new writer. An idempotent re-bind of
    /// a connection already in the registry is not an event: treating
    /// it as one would lower CB_PATH_DOWN on traffic over the very
    /// connection whose send just failed.
    pub fn note_back_channel_bound(&self, session_id: &crate::nfs::v4::protocol::SessionId) {
        let Some(client_id) = self.sessions.get_session(session_id).map(|s| s.client_id) else {
            return;
        };
        self.lower_seq_flags(
            client_id,
            crate::nfs::v4::protocol::seq4_status::CB_PATH_DOWN,
        );
        self.delegations.note_rearm(client_id);
    }

    /// Clear SEQ4_STATUS_* bits for a client (the condition resolved —
    /// e.g. its last revoked delegation was freed, or a backchannel
    /// rebind ended CB_PATH_DOWN).
    pub fn lower_seq_flags(&self, client_id: u64, bits: u32) {
        if let Some(mut e) = self.seq_flags.get_mut(&client_id) {
            *e &= !bits;
        }
    }

    /// The bits a SEQUENCE reply to this client must carry right now.
    pub fn seq_flags(&self, client_id: u64) -> u32 {
        self.seq_flags.get(&client_id).map(|e| *e).unwrap_or(0)
    }

    /// Test/dev convenience: build a `StateManager` over a fresh
    /// `MemoryBackend`. Equivalent to `new(volume_id,
    /// memory_backend())` — makes the call sites in `#[cfg(test)]`
    /// modules read tighter.
    pub fn new_in_memory(volume_id: &str) -> Self {
        Self::new(volume_id, crate::state_backend::memory_backend())
    }

    /// Test constructor: an in-memory StateManager with explicit
    /// [`StateQuotas`] — quota tests need tiny deterministic caps, and
    /// the env-derived defaults are process-global.
    pub fn new_in_memory_with_quotas(volume_id: &str, quotas: StateQuotas) -> Self {
        let mut mgr = Self::new(volume_id, crate::state_backend::memory_backend());
        mgr.quotas = quotas;
        mgr
    }

    /// Pre-listener hook: pull every persisted record out of the
    /// backend and seed the in-memory caches with it. After this
    /// returns, hot-path reads through `clients` / `sessions` /
    /// `stateids` find their pre-restart records — clients
    /// reconnecting against the post-restart MDS see no
    /// `STALE_CLIENTID` / `BAD_STATEID`. `LayoutManager` is loaded
    /// separately by the pNFS startup path because it lives outside
    /// the NFSv4 `state` module.
    /// `state_lost` is the caller's "a prior state database existed and
    /// could not be used" signal (a quarantined-and-recreated `state.db`).
    /// It is the difference between the two ways a load comes back empty,
    /// and they need opposite handling — see the grace decision below.
    pub async fn load_from_backend(&self, state_lost: bool) -> Result<(), StateBackendError> {
        let clients = self.backend.list_clients().await?;
        let sessions = self.backend.list_sessions().await?;
        let stateids = self.backend.list_stateids().await?;
        let n_c = clients.len();
        let n_s = sessions.len();
        let n_st = stateids.len();
        self.clients.load_records(clients);
        self.sessions.load_records(sessions);
        let markers = self.stateids.load_records(stateids);
        // §10 layer 3: a trip that was in force when the predecessor
        // died is restored for the REMAINDER of its window. Without
        // this, a roll during a revocation storm comes back granting
        // freely into the same storm — the breaker would protect
        // against everything except the response to the incident.
        if let Some(trip_unix) = markers.breaker_trip_unix {
            if self.delegations.restore_breaker_trip(trip_unix) {
                tracing::warn!(
                    "grant breaker was TRIPPED when this hub's predecessor died \
                     (at unix {}) — new delegation grants stay paused for the \
                     remainder of the window",
                    trip_unix
                );
            } else {
                tracing::info!(
                    "a breaker trip from unix {} was found but its window has \
                     expired — not restored",
                    trip_unix
                );
            }
        }
        // Holder evidence (design §6): a marker row means a client
        // held recallable state when this incarnation's predecessor
        // died — and a same-PVC restart is TRANSPARENT to it
        // (EXCHANGE_ID case 1, no CLAIM_PREVIOUS), so without this
        // bit it would serve its page cache forever against a server
        // that forgot the delegation. Pre-arm SEQ4 so its first lease
        // renewal tells it to drop and revalidate.
        for client_id in markers.deleg_holders {
            tracing::warn!(
                "client {} held recallable state across the restart — \
                 pre-arming SEQ4_STATUS_RECALLABLE_STATE_REVOKED",
                client_id
            );
            self.raise_seq_flags(
                client_id,
                crate::nfs::v4::protocol::seq4_status::RECALLABLE_STATE_REVOKED,
            );
            self.armed_markers.insert(client_id, None);
        }
        tracing::info!(
            "StateManager loaded {} clients, {} sessions, {} stateids from backend",
            n_c,
            n_s,
            n_st,
        );
        // Grace is a WINDOW and "can anyone reclaim" is a FACT, and the
        // two were previously collapsed into one decision: an empty load
        // ended grace outright. That is wrong in both directions.
        //
        // Ending it is unsafe when the emptiness is not innocent. A
        // quarantined-and-recreated state.db also loads zero of
        // everything, and that is exactly when clients may hold opens and
        // byte-range locks this incarnation has no record of — ending
        // grace lets a second client take a range whose holder has been
        // forgotten, silently, because the lock stateid still validates.
        //
        // Keeping it is wrong for callers that cannot reclaim by
        // construction. The hub's own file API dispatches in-process with
        // minor_version 0 and no session, so it can never be "reclaim
        // complete"; holding it in grace is a window of refused writes on
        // every hibernate wake while reads serve normally — browsing
        // works and saving does not.
        //
        // So the window now always runs (RFC 8881 §18.51.3 wants
        // NFS4ERR_GRACE for an unreclaimed 4.1 client, pynfs RECC3), and
        // the fact is recorded separately for the OPEN gate to consult.
        let anything_reclaimable = state_lost || n_c > 0 || n_s > 0 || n_st > 0;
        self.leases.set_anything_reclaimable(anything_reclaimable);
        if state_lost {
            tracing::warn!(
                "state was LOST (prior database unusable) — treating this volume \
                 as reclaimable even though nothing loaded: clients may hold opens \
                 and locks this incarnation cannot see"
            );
        }
        Ok(())
    }

    /// Borrow the shared backend (used by the pNFS layer to load
    /// `LayoutManager` records and to share an instance counter).
    pub fn backend(&self) -> Arc<dyn StateBackend> {
        Arc::clone(&self.backend)
    }

    /// Cleanup expired state
    ///
    /// Removes expired leases and their associated sessions and clients.
    /// This should be called periodically (e.g., every 30 seconds) to prevent
    /// resource leaks from clients that stop responding.
    ///
    /// Prefer [`Self::cleanup_expired_ids`] when the caller has ALREADY read
    /// `get_expired_clients()` and acted on it. Re-reading it here is a
    /// second, independent decision about who is expired, and `renew_lease`
    /// is documented LOCK-FREE ("per-client locking only, not global") — so
    /// a SEQUENCE arriving between the two reads changes the answer. See the
    /// note on `cleanup_expired_ids`.
    pub fn cleanup_expired(&self) {
        let expired_clients = self.leases.get_expired_clients();
        self.cleanup_expired_ids(&expired_clients);
    }

    /// Retire exactly the clients the caller decided were expired.
    ///
    /// The `&[u64]` is the whole point. `courtesy_release_expired` used to
    /// call `cleanup_expired()`, which re-read `get_expired_clients()` and so
    /// made its OWN decision about who was expired — while the caller had
    /// already stripped those clients' locks against the first reading.
    /// `renew_lease` is lock-free, so a SEQUENCE landing between the two
    /// reads renews the lease and the second reading no longer sees the
    /// client. Its locks were already gone; its session, stateids and client
    /// record survived; and `status_flags` is hardcoded 0, so nothing told
    /// it. That is a client still holding a session and still believing it
    /// owns a byte range the server has already handed to someone else.
    ///
    /// Found by TLC — `formal/FlintClientIdentityLeaseSilent.cfg`, which
    /// reproduces it as LeaseLapse → SweepLocks → Sequence → SweepState. The
    /// counterexample needs a SECOND agent, because the sweep runs at the top
    /// of every COMPOUND and reaps EVERY expired client, not the caller's: it
    /// is the other cluster's traffic that strips this cluster's locks.
    ///
    /// Following through on the first reading means a client that renews at
    /// exactly the wrong moment loses its session and must recover. That is a
    /// real cost and it is the right trade: the alternative is not "it keeps
    /// working", it is "it keeps working WITHOUT the locks it thinks it
    /// holds". BADSESSION is a path every NFS client already implements.
    pub fn cleanup_expired_ids(&self, expired_clients: &[u64]) {
        // NOTE there is deliberately no `self.leases.cleanup_expired()`
        // here. That call retires every CURRENTLY-expired lease, which is a
        // wider set than the snapshot: a client whose lease lapsed after the
        // snapshot was taken would lose its lease here and never appear in
        // `expired_clients`, so its record, stateids and locks would not be
        // cleaned — and it could never be swept again either, because
        // `get_expired_clients()` iterates leases and it no longer has one.
        // `remove_client` drops each client's lease below, which is exactly
        // scoped to the snapshot.
        for &client_id in expired_clients {
            // Destroy all sessions for this client
            self.sessions.destroy_client_sessions(client_id);

            // RFC 8881 §8.4.2.4 courtesy-release: a client whose
            // lease has expired forfeits its share-reservations and
            // open state to the next conflicting op. Wiping the
            // stateids + open-state records here lets that next op
            // proceed instead of getting blocked on a phantom
            // conflict. Locks held by this client are released by
            // the dispatcher's courtesy-cleanup hook (since
            // `LockManager` lives outside `StateManager`).
            self.stateids.remove_client_stateids(client_id);

            // Remove the client itself
            self.clients.remove_client(client_id);

            // Cleanup any delegations for this client. The freed
            // stateids' master entries were already dropped by
            // remove_client_stateids above.
            let _ = self.delegations.cleanup_client_delegations(client_id);
        }
    }
}

impl Default for StateManager {
    fn default() -> Self {
        // Default is in-memory only — no restart survival. Production
        // callers should use `StateManager::new(volume_id, sqlite)`.
        Self::new_in_memory("")
    }
}

#[cfg(test)]
mod tests {
    /// The matrix above drives `deleg_fence` DIRECTLY, so it proves the
    /// funnel attributes whatever label it is handed — not that the
    /// call sites hand it the right one. This closes that gap from the
    /// other side: every site literal that appears at a real fence call
    /// in the source must be one the matrix exercises, and vice versa.
    ///
    /// A new conflict site added with an unexercised label fails here;
    /// so does a typo, which would otherwise show up only as a §9
    /// per-site equality that is quietly always zero.
    #[test]
    fn every_fence_call_site_uses_a_label_the_matrix_exercises() {
        const SITES: &[&str] = &[
            "open_create", "open_write", "write", "setattr", "remove",
            "rename_src", "rename_dst", "link", "allocate", "deallocate",
            "copy_dst", "clone_dst", "lock_write",
            // MDS-posture lanes (slice 5): a write-capable LAYOUTGET, the
            // LAYOUTCOMMIT that publishes DS-written bytes, and the
            // proxied fallback WRITE that never reaches ioops.
            "layoutget_rw", "layoutcommit", "write_proxy",
        ];
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/nfs/v4");
        let mut found: std::collections::BTreeSet<String> = Default::default();
        let mut files = Vec::new();
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for e in std::fs::read_dir(dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    out.push(p);
                }
            }
        }
        walk(&base, &mut files);
        // `site` is always the LAST argument of a fence call, and every
        // call site passes it as a literal — a computed label would not
        // be greppable, which is itself a reason to keep them literal.
        let re_call = regex::Regex::new(
            r#"deleg_fence(?:_path|_ident)?\s*\([^;]*?"([a-z_]+)"[^;]*?\)"#,
        )
        .unwrap();
        for f in &files {
            // The matrix and this test both list the labels; skip the
            // state module so its own SITES arrays are not mistaken for
            // call sites.
            if f.ends_with("state/mod.rs") {
                continue;
            }
            let src = std::fs::read_to_string(f).unwrap();
            for c in re_call.captures_iter(&src) {
                found.insert(c[1].to_string());
            }
        }
        assert!(!found.is_empty(), "the scan found no fence call sites at all — \
                 a scan that cannot fail proves nothing");
        for site in &found {
            assert!(
                SITES.contains(&site.as_str()),
                "fence call site uses label {site:?}, which the conflict-site \
                 matrix does not exercise — add it to SITES in both tests"
            );
        }
        // `allocate`/`deallocate` are derived from AllocMode rather than
        // written at the call, so they are legitimately absent here.
        for expect in ["write", "setattr", "remove", "rename_src", "link"] {
            assert!(
                found.contains(expect),
                "expected a fence call site labelled {expect:?}; found {found:?}"
            );
        }
    }

    /// THE CONFLICT-SITE MATRIX (design §9), behavioural half.
    ///
    /// `every_f14_bump_lane_is_fenced_or_exempted` proves every
    /// mutating lane CONTAINS a fence consult. It cannot prove the
    /// consult does anything: a site could pass a label the funnel
    /// ignores, or fence an identity the mutation does not touch, and
    /// the source scan would still be green.
    ///
    /// So this drives each §5.2 site label through the real funnel
    /// against a live foreign delegation and asserts the three facts
    /// the design's per-site equalities rest on: B's FIRST attempt is
    /// DELAYed, the delay is attributed to THAT site, and after the
    /// holder returns, B's retry proceeds.
    #[test]
    fn every_conflict_site_delays_then_proceeds_after_the_return() {
        let _flag = with_delegations(true);
        // Every site named in §5.2 that reaches the shared funnel.
        // Adding a site to `deleg_fence` without adding it here leaves
        // its label unexercised, which is how a mislabelled delay
        // survives review.
        const SITES: &[&str] = &[
            "open_create", "open_write", "write", "setattr", "remove",
            "rename_src", "rename_dst", "link", "allocate", "deallocate",
            "copy_dst", "clone_dst", "lock_write",
            // MDS-posture lanes (slice 5): a write-capable LAYOUTGET, the
            // LAYOUTCOMMIT that publishes DS-written bytes, and the
            // proxied fallback WRITE that never reaches ioops.
            "layoutget_rw", "layoutcommit", "write_proxy",
        ];

        for (n, site) in SITES.iter().enumerate() {
            let sm = std::sync::Arc::new(StateManager::new_in_memory(""));
            sm.install_recall_spawner(Arc::new(|_| {}));
            let dev = 40 + n as u64;
            let ident = FileId::new(dev, 1);
            sm.leases.create_lease(7);
            let sid = crate::nfs::v4::protocol::StateId {
                seqid: 1,
                other: [n as u8 + 1; 12],
            };
            sm.delegations
                .try_grant(ident, 7, vec![0xf], std::path::PathBuf::from("/m"), || true, || sid)
                .unwrap();

            // Client 8 mutates: first attempt must DELAY.
            let v = sm.deleg_fence((dev, 1), Some(8), false, site);
            assert!(
                matches!(v, FenceVerdict::Delay),
                "site {site}: a foreign live delegation must DELAY the mutator, got {v:?}"
            );
            assert_eq!(
                sm.delegations.meter().delay_count(site),
                1,
                "site {site}: the delay must be attributed to THIS site —                  §9 asserts per-site equalities, so a mislabelled delay turns a                  real regression into a green leg somewhere else"
            );

            // The holder returns; the barrier lifts and the retry runs.
            sm.delegations.return_delegation(&sid).unwrap();
            let v2 = sm.deleg_fence((dev, 1), Some(8), false, site);
            assert!(
                matches!(v2, FenceVerdict::Proceed(_)),
                "site {site}: after DELEGRETURN the retry must proceed, got {v2:?}"
            );
            assert_eq!(
                sm.delegations.meter().delay_count(site),
                1,
                "site {site}: the retry must not be counted as a second delay"
            );
        }
    }

    /// ANTI-VACUITY for the matrix above: the SAME client mutating its
    /// own delegated file is never delayed (the §4 self-conflict
    /// carve-out). Without this leg, the matrix passes against a fence
    /// that delays everything — which would deadlock every holder
    /// against its own writes.
    #[test]
    fn a_holder_mutating_its_own_file_is_never_delayed() {
        let _flag = with_delegations(true);
        let sm = std::sync::Arc::new(StateManager::new_in_memory(""));
        sm.install_recall_spawner(Arc::new(|_| {}));
        let ident = FileId::new(99, 1);
        sm.leases.create_lease(7);
        let sid = crate::nfs::v4::protocol::StateId { seqid: 1, other: [77u8; 12] };
        sm.delegations
            .try_grant(ident, 7, vec![0xf], std::path::PathBuf::from("/self"), || true, || sid)
            .unwrap();

        let v = sm.deleg_fence((99, 1), Some(7), false, "write");
        assert!(
            matches!(v, FenceVerdict::Proceed(_)),
            "the holder writing its OWN delegated file must not wait on itself, got {v:?}"
        );
        assert_eq!(sm.delegations.meter().delay_count("write"), 0);
    }

    /// The expired-courtesy short-circuit (design §5.3 graft).
    ///
    /// A holder whose lease has lapsed cannot answer a recall, so
    /// making the mutator wait out the 90s ladder for it is 90s of
    /// DELAY spent on a client that is gone.
    #[test]
    fn an_expired_holder_is_revoked_at_the_consult_instead_of_recalled() {
        use crate::nfs::v4::protocol::seq4_status;
        use crate::nfs::v4::state::deleg_meter::RevokeReason;
        // deleg_fence is gated on BOTH the flag and an installed recall
        // spawner — without them it proceeds without consulting the
        // table at all, and this test would pass while exercising
        // nothing.
        let _flag = with_delegations(true);
        let sm = std::sync::Arc::new(StateManager::new_in_memory(""));
        sm.install_recall_spawner(Arc::new(|_orders| {}));
        let ident = FileId::new(1, 1);
        sm.leases.create_lease(7);
        let sid = crate::nfs::v4::protocol::StateId { seqid: 1, other: [7u8; 12] };
        sm.delegations
            .try_grant(ident, 7, vec![0xf], std::path::PathBuf::from("/f"), || true, || sid)
            .unwrap();
        sm.leases.expire_now(7);

        // A different client mutates.
        let verdict = sm.deleg_fence((1, 1), Some(8), false, "write");
        assert!(
            matches!(verdict, FenceVerdict::Proceed(_)),
            "the writer must NOT wait on a client whose lease already lapsed"
        );
        assert_eq!(
            sm.delegations.snapshot(&sid).unwrap().state,
            delegation::DelegState::Revoked
        );
        // Loud, never silent — this is what makes the lease-check race
        // against a renewal safe rather than merely unlikely.
        assert_ne!(
            sm.seq_flags(7) & seq4_status::RECALLABLE_STATE_REVOKED,
            0,
            "an expired-holder revoke must still raise SEQ4"
        );
        let m = sm.delegations.meter();
        assert_eq!(m.revoked_count(RevokeReason::LeaseExpired), 1);
        assert_eq!(
            m.revoked_count(RevokeReason::ChannelDead),
            0,
            "a lapsed lease is not a broken back-channel — collapsing them \
             would hide a real reachability failure inside client churn"
        );
    }

    /// ANTI-VACUITY: a LIVE holder is still recalled and still delays
    /// the mutator. Without this, the test above passes against a
    /// fence that short-circuits everything — which would revoke every
    /// delegation on first conflict and make the feature pointless.
    #[test]
    fn a_live_holder_is_recalled_and_the_mutator_waits() {
        let sm = std::sync::Arc::new(StateManager::new_in_memory(""));
        let ident = FileId::new(2, 2);
        sm.leases.create_lease(7);
        let sid = crate::nfs::v4::protocol::StateId { seqid: 1, other: [9u8; 12] };
        sm.delegations
            .try_grant(ident, 7, vec![0xf], std::path::PathBuf::from("/g"), || true, || sid)
            .unwrap();

        match sm.delegations.mutation_fence(ident, Some(8), false) {
            FenceOutcome::Conflict { recalls, expired, delay, .. } => {
                assert_eq!(recalls.len(), 1, "a live holder gets a real recall");
                assert!(expired.is_empty());
                assert!(delay, "and the mutator waits for it");
            }
            FenceOutcome::Clear(_) => panic!("expected a conflict"),
        }
    }

    /// A client with NO lease row is not short-circuited. Absence is
    /// ambiguous — swept, or never created on this path — and absence
    /// is not evidence. The ladder discovers an unreachable client on
    /// its own via path_down, which is slower and correct.
    #[test]
    fn a_holder_with_no_lease_row_is_not_treated_as_expired() {
        let sm = std::sync::Arc::new(StateManager::new_in_memory(""));
        let ident = FileId::new(3, 3);
        let sid = crate::nfs::v4::protocol::StateId { seqid: 1, other: [11u8; 12] };
        sm.delegations
            .try_grant(ident, 7, vec![0xf], std::path::PathBuf::from("/h"), || true, || sid)
            .unwrap();
        match sm.delegations.mutation_fence(ident, Some(8), false) {
            FenceOutcome::Conflict { recalls, expired, .. } => {
                assert_eq!(recalls.len(), 1);
                assert!(expired.is_empty(), "no lease row is not a lapsed lease");
            }
            FenceOutcome::Clear(_) => panic!("expected a conflict"),
        }
    }

    use super::*;

    /// An empty load must record whether anything is RECLAIMABLE — and
    /// must not end the grace WINDOW to say it.
    ///
    /// Both arms load exactly nothing, so a check that looked only at
    /// the record counts cannot tell them apart. `state_lost` is the
    /// difference, and the two need opposite answers:
    ///
    /// - fresh volume / hibernate wake: nothing ever existed, so a
    ///   caller that cannot reclaim by construction (the in-process file
    ///   API, no session, no RECLAIM_COMPLETE in 4.0) has nothing to
    ///   wait for and must not be held — otherwise every wake refuses
    ///   writes for the whole window while reads serve.
    /// - quarantined state.db: the tables are empty because the state
    ///   was LOST. Clients may still hold opens and byte-range locks
    ///   this incarnation cannot see, so everything waits.
    ///
    /// In BOTH arms the grace window itself keeps running: a 4.1 client
    /// that has not sent RECLAIM_COMPLETE must still get NFS4ERR_GRACE
    /// (RFC 8881 §18.51.3, pynfs RECC3). Ending the window was the old
    /// conflation of "nobody can reclaim" with "grace is over".
    #[tokio::test]
    async fn an_empty_load_records_reclaimability_without_ending_grace() {
        // Innocent empty: nothing was ever there.
        let fresh = StateManager::new_in_memory("vol");
        assert!(
            fresh.leases.anything_reclaimable(),
            "before any load the server must assume the worst"
        );
        fresh.load_from_backend(false).await.expect("empty load must succeed");
        assert!(
            !fresh.leases.anything_reclaimable(),
            "a hub with nothing to reclaim must say so, or the file API is held \
             for the whole window and the hibernate wake refuses every write"
        );
        assert!(
            fresh.leases.in_grace_period(),
            "the grace WINDOW still runs — a 4.1 client that has not reclaimed \
             must still see NFS4ERR_GRACE (RECC3). Ending it here was the bug."
        );

        // Guilty empty: a prior database existed and could not be used.
        // Identical record counts; opposite required answer.
        let lost = StateManager::new_in_memory("vol");
        lost.load_from_backend(true).await.expect("empty load must succeed");
        assert!(
            lost.leases.anything_reclaimable(),
            "state was LOST, so empty tables prove nothing — everything waits, or \
             a second client takes a range whose holder we have forgotten"
        );
        assert!(lost.leases.in_grace_period(), "and the window runs here too");
    }

    #[test]
    fn test_cleanup_expired_removes_clients_and_sessions() {
        let state_mgr = StateManager::new_in_memory("");

        // Create a client and session
        let outcome = state_mgr.clients.exchange_id(b"test-client".to_vec(), 12345, 0, Vec::new());
        let client_id = match outcome {
            crate::nfs::v4::state::client::ExchangeIdOutcome::NewUnconfirmed { client_id, .. } => client_id,
            other => panic!("expected NewUnconfirmed, got {:?}", other),
        };

        // Create session for this client
        let session = state_mgr.sessions.create_session(
            client_id,
            0,
            0,
            10,
            4096, 4096, 8,
            10,
            0,
            None,
            1,
        );

        // Verify client and session exist
        assert!(state_mgr.clients.get_client(client_id).is_some());
        assert!(state_mgr.sessions.get_session(&session.session_id).is_some());
        assert_eq!(state_mgr.clients.active_count(), 1);
        assert_eq!(state_mgr.sessions.active_count(), 1);

        // Note: We can't easily test actual lease expiration without waiting 90+ seconds,
        // so this test verifies the cleanup logic doesn't crash with active leases
        state_mgr.cleanup_expired();

        // Since no leases have expired, clients and sessions should still exist
        assert_eq!(state_mgr.clients.active_count(), 1);
        assert_eq!(state_mgr.sessions.active_count(), 1);
    }

    /// A SWEEP THAT HAS STARTED MUST FINISH, even if the client renews.
    ///
    /// `courtesy_release_expired` reads `get_expired_clients()` and strips
    /// those clients' locks. It then used to call `cleanup_expired()`, which
    /// re-read `get_expired_clients()` — a SECOND, independent decision about
    /// who was expired. `renew_lease` is documented LOCK-FREE ("per-client
    /// locking only, not global"), so a SEQUENCE landing between the two
    /// reads renews the lease and the second read no longer sees the client:
    /// its locks are already gone, its session and stateids survive, and
    /// `status_flags` is hardcoded 0 so nothing tells it. The client goes on
    /// believing it holds a byte range the server has handed to someone else.
    ///
    /// This test needs no expired lease at all, and that is the point: a
    /// perfectly healthy lease IS the post-renewal state. Passing the id in
    /// the snapshot must be enough to retire it.
    ///
    /// Found by TLC — `formal/FlintClientIdentityLeaseSilent.cfg` walks it as
    /// LeaseLapse -> SweepLocks -> Sequence -> SweepState. The counterexample
    /// needs a SECOND agent, because the sweep runs at the top of every
    /// COMPOUND and reaps EVERY expired client rather than the caller's: with
    /// several clusters on one hub it is the other cluster's traffic that
    /// strips this cluster's locks.
    #[test]
    fn a_sweep_must_follow_through_on_the_reading_it_started_from() {
        let state_mgr = StateManager::new_in_memory("");
        let mk = |owner: &[u8]| match state_mgr.clients.exchange_id(owner.to_vec(), 1, 0, Vec::new()) {
            crate::nfs::v4::state::client::ExchangeIdOutcome::NewUnconfirmed { client_id, .. } => client_id,
            other => panic!("expected NewUnconfirmed, got {:?}", other),
        };
        let renewer = mk(b"Linux NFSv4.2 agent-a");
        let bystander = mk(b"Linux NFSv4.2 agent-b");

        // The snapshot the sweep took. `renewer` was expired when it was
        // read; by now it has renewed, so its lease is live again — which is
        // exactly the state this assertion is made against.
        assert!(
            state_mgr.leases.is_valid(renewer),
            "the renewal has landed: this lease is live, and the old code \
             would therefore have skipped it",
        );
        state_mgr.cleanup_expired_ids(&[renewer]);

        assert!(
            state_mgr.clients.get_client(renewer).is_none(),
            "a client whose locks were already stripped must be retired too — \
             skipping it is not 'it keeps working', it is 'it keeps working \
             without the locks it thinks it holds'",
        );

        // ANTI-VACUITY: the snapshot must be respected in BOTH directions.
        // A cleanup that simply retired everything would pass the assertion
        // above and be far worse than the defect.
        assert!(
            state_mgr.clients.get_client(bystander).is_some(),
            "a client the sweep never read must be untouched",
        );
        assert!(state_mgr.leases.is_valid(bystander), "and must keep its lease");
    }

    #[test]
    fn test_cleanup_expired_with_no_expired_clients() {
        let state_mgr = StateManager::new_in_memory("");

        // Create a client and session
        let outcome = state_mgr.clients.exchange_id(b"test-client".to_vec(), 12345, 0, Vec::new());
        let client_id = match outcome {
            crate::nfs::v4::state::client::ExchangeIdOutcome::NewUnconfirmed { client_id, .. } => client_id,
            other => panic!("expected NewUnconfirmed, got {:?}", other),
        };

        state_mgr.sessions.create_session(
            client_id,
            0,
            0,
            10,
            4096, 4096, 8,
            10,
            0,
            None,
            1,
        );

        // Run cleanup (no leases have expired)
        state_mgr.cleanup_expired();

        // Client and session should still exist
        assert_eq!(state_mgr.clients.active_count(), 1);
        assert_eq!(state_mgr.sessions.active_count(), 1);
    }

    #[test]
    fn test_get_expired_clients_returns_empty_for_active_leases() {
        let state_mgr = StateManager::new_in_memory("");

        // Create a client with active lease
        let outcome = state_mgr.clients.exchange_id(b"test-client".to_vec(), 12345, 0, Vec::new());
        let client_id = match outcome {
            crate::nfs::v4::state::client::ExchangeIdOutcome::NewUnconfirmed { client_id, .. } => client_id,
            other => panic!("expected NewUnconfirmed, got {:?}", other),
        };

        // Verify no expired clients
        let expired = state_mgr.leases.get_expired_clients();
        assert_eq!(expired.len(), 0);

        // Verify client still exists
        assert!(state_mgr.clients.get_client(client_id).is_some());
    }

    #[test]
    fn test_state_manager_default() {
        let state_mgr = StateManager::default();
        assert_eq!(state_mgr.clients.active_count(), 0);
        assert_eq!(state_mgr.sessions.active_count(), 0);
    }

    /// **The whole point of B.3.** Build a StateManager, stuff it
    /// with state, mutate that state, then build a *fresh*
    /// StateManager backed by the same `MemoryBackend` and prove
    /// `load_from_backend` reconstructs the in-memory caches —
    /// active client_id is back, mark_confirmed was persisted, the
    /// session is bound to the same client.
    ///
    /// This is the test that B.5's Lima e2e (`make
    /// test-pnfs-restart`) will mirror at the process level. If
    /// this passes, the in-process plumbing is sound and the
    /// remaining work is plumbing config + the e2e harness.
    /// Every hub that does not share a client table MUST advertise a
    /// unique `server_owner`. The kernel's `nfs4_detect_session_trunking`
    /// treats same-owner servers as ONE server reachable at several
    /// addresses and requires EXCHANGE_ID to return the same clientid on
    /// every one — which two independent hubs, each counting from 1, can
    /// only satisfy by coincidence.
    ///
    /// The pNFS MDS passed `""` here, so EVERY flint-lite hub advertised
    /// the constants `flint-nfs` / `flint-nfs-standalone`. An agent
    /// mounting two workspaces therefore presented the kernel with one
    /// identity at two addresses. It now passes its PERSISTENT server id
    /// (`get_or_init_server_id`, which lives in the state.db on the PVC,
    /// so it is unique per volume and stable across restarts).
    #[test]
    fn two_hubs_must_not_advertise_the_same_server_identity() {
        use crate::state_backend::MemoryBackend;
        // The mds arm builds its owner from the shard id and ignores
        // volume_id entirely, which would make this vacuous.
        assert!(
            std::env::var("PNFS_MODE").is_err(),
            "this test only means something in the standalone arm"
        );
        let b1: Arc<dyn StateBackend> = Arc::new(MemoryBackend::new());
        let b2: Arc<dyn StateBackend> = Arc::new(MemoryBackend::new());
        let a = StateManager::new("16435955404748484869", b1);
        let b = StateManager::new("8295498220999890219", b2);

        assert_ne!(
            a.clients.server_owner(),
            b.clients.server_owner(),
            "two hubs advertised one server_owner — a client mounting both would \
             demand clientid parity they can only meet by coincidence"
        );
        assert_ne!(
            a.clients.server_scope(),
            b.clients.server_scope(),
            "two hubs advertised one server_scope"
        );
    }

    /// The trap itself, pinned so the next caller sees it: an EMPTY
    /// volume_id collapses every hub onto one shared identity. This is
    /// what the MDS did, and it is why `server.rs` now passes the
    /// persistent server id rather than `""`.
    #[test]
    fn an_empty_volume_id_collapses_every_hub_onto_one_identity() {
        use crate::state_backend::MemoryBackend;
        assert!(
            std::env::var("PNFS_MODE").is_err(),
            "this test only means something in the standalone arm"
        );
        let b1: Arc<dyn StateBackend> = Arc::new(MemoryBackend::new());
        let b2: Arc<dyn StateBackend> = Arc::new(MemoryBackend::new());
        let a = StateManager::new("", b1);
        let b = StateManager::new("", b2);
        assert_eq!(a.clients.server_owner(), b.clients.server_owner());
        assert_eq!(a.clients.server_owner(), "flint-nfs");
        assert_eq!(a.clients.server_scope(), b"flint-nfs-standalone");
    }

    #[tokio::test]
    async fn test_state_manager_reload_from_shared_backend() {
        use crate::state_backend::MemoryBackend;

        // Phase 1: write phase. Tokio runtime is live, so the
        // fire-and-forget persist tasks actually run.
        let backend: Arc<dyn StateBackend> = Arc::new(MemoryBackend::new());
        let mgr1 = StateManager::new("vol1", Arc::clone(&backend));
        let outcome = mgr1.clients.exchange_id(
            b"alice-client".to_vec(),
            0xc0ffee,
            0,
            b"alice@FLINT".to_vec(),
        );
        let client_id = match outcome {
            crate::nfs::v4::state::client::ExchangeIdOutcome::NewUnconfirmed {
                client_id,
                ..
            } => client_id,
            other => panic!("expected NewUnconfirmed, got {:?}", other),
        };
        mgr1.clients.mark_confirmed(client_id);
        let session = mgr1.sessions.create_session(
            client_id, 0, 0, 4096, 4096, 1024, 16, 8, 0xcb_aabb,
            None,
            1,
        );

        // Persist is fire-and-forget; let the spawned tasks land
        // before we read the backend. In production this is bounded
        // by tokio's task queue; in tests we yield once.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        // The spawned put_client/put_session/etc. complete on the
        // next runtime tick; allow a small budget rather than
        // racing.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Sanity: backend has what we put.
        assert_eq!(backend.list_clients().await.unwrap().len(), 1);
        assert_eq!(backend.list_sessions().await.unwrap().len(), 1);

        // Phase 2: simulate a restart. New StateManager, same
        // backend, then load_from_backend. Equivalent at the
        // protocol level to "MDS pod rolled, comes back, kernel
        // reconnects against the same client_id".
        drop(mgr1);
        let mgr2 = StateManager::new("vol1", Arc::clone(&backend));
        // Pre-load: cache is empty.
        assert_eq!(mgr2.clients.active_count(), 0);
        assert_eq!(mgr2.sessions.active_count(), 0);

        mgr2.load_from_backend(false).await.expect("load must succeed");

        // Post-load: client is back with mark_confirmed intact —
        // EXCHANGE_ID after restart will return this same client_id
        // (case 1 of RFC 8881 §18.35.5) so the kernel keeps using
        // its existing client_id.
        let restored = mgr2
            .clients
            .get_client(client_id)
            .expect("client must reload");
        assert!(restored.confirmed, "mark_confirmed must persist");
        assert_eq!(restored.owner, b"alice-client");
        assert_eq!(restored.principal, b"alice@FLINT");

        // Sessions are deliberately NOT restored to the live map.
        // Slot replay state can't survive restart (RFC 8881
        // §15.1.10.4), so reloading a session would break Linux
        // clients that send SEQUENCE with their current per-slot
        // seqid. Instead, the kernel sees its session_id is unknown
        // → BADSESSION → CREATE_SESSION fresh → resumes against
        // the same persisted client_id. See
        // `SessionManager::load_records` for the full rationale.
        assert_eq!(
            mgr2.sessions.active_count(),
            0,
            "sessions deliberately not restored — kernel re-CREATE_SESSIONs",
        );
        // The persisted-id counter still got bumped past `session.session_id`'s
        // numeric component so a fresh CREATE_SESSION never collides.
        let new_session = mgr2.sessions.create_session(
            client_id, 0, 0, 4096, 4096, 1024, 16, 8, 0xcb_aabb,
            None,
            1,
        );
        assert_ne!(
            new_session.session_id, session.session_id,
            "post-restart CREATE_SESSION must mint a fresh session_id",
        );
    }

    /// Grant rule 6's posture semantics (slice 5). Outside the MDS
    /// posture no layout can exist and the rule is a constant false;
    /// inside it the rule consults the probe, and WITHOUT a probe it
    /// fails closed — an MDS that grants without consulting its
    /// layouts is the silent-stale case, so the absent wiring must
    /// refuse rather than allow.
    #[test]
    fn rule_6_is_false_standalone_and_fails_closed_in_mds_posture_without_a_probe() {
        let sm = StateManager::new_in_memory("");
        assert!(!sm.write_layout_held_by_other("f", 1), "standalone: no layouts, no conflict");
        assert!(!sm.layout_probe_installed());

        sm.set_pnfs_posture();
        assert!(
            sm.write_layout_held_by_other("f", 1),
            "MDS posture with no probe must REFUSE, not allow"
        );

        let holders: Arc<std::sync::Mutex<Vec<u64>>> = Arc::new(std::sync::Mutex::new(vec![1, 2]));
        let h = Arc::clone(&holders);
        sm.install_layout_probe(Arc::new(move |_key: &str| h.lock().unwrap().clone()));
        assert!(sm.layout_probe_installed());
        assert!(sm.write_layout_held_by_other("f", 1), "client 2's layout conflicts with 1's grant");
        *holders.lock().unwrap() = vec![1];
        assert!(!sm.write_layout_held_by_other("f", 1), "the requester's OWN layout does not");
        holders.lock().unwrap().clear();
        assert!(!sm.write_layout_held_by_other("f", 1));
    }
}
