//! THE COMPOSITION WITNESS — the third point of view a two-copy volume
//! needs, and the store `FlintComposition.tla` has been modelling since
//! its first tranche without anybody noticing.
//!
//! # Why this exists
//!
//! A composition is one volume with two copies on two targets, and a
//! failover is the decision *which target may serve it*. That decision
//! has to be made by exactly one thing, or it is not a decision: the
//! model's whole machine — `PromoteCAS`, the eviction horizon,
//! `ElectInSync`, the dead-man — rests on a single record advanced by a
//! single CAS.
//!
//! The block tier had no such record. MDS shards share nothing (own
//! sqlite, own RWO PVC, single-writer by attach), so a two-copy volume
//! had TWO records, and `the_leg_host_cannot_elect_itself_…` (block_
//! export.rs) proves what that costs: every fact the survivor's election
//! reads — the composer's dial coordinates, the survivor's own leg and
//! its in-sync mark — is a fact the composer wrote in its OWN database.
//! The survivor cannot even observe the outage, let alone act on it.
//!
//! Two participants cannot distinguish peer-death from partition, so no
//! amount of protocol between two records fixes this; safe automatic
//! failover needs a third party. The model says so with teeth:
//! `FlintCompositionLocalMark.cfg` is peer-arbitration's degraded window
//! as a counterexample — the composer marks its peer stale in the record
//! it can always write (its own), the election reads the record that was
//! never told, and acked writes are discarded in good faith.
//!
//! # What a witness is
//!
//! Exactly one property, and it is the one tranches 1-3 quietly assumed:
//! **a store both targets can reach independently of each other's
//! health.** `PromoteCAS` carries no reachability guard on the record,
//! `LeaseLapse` observes expiry spontaneously, `MarkStale` is guarded on
//! the ACTOR's health alone. Sqlite on one of the two targets cannot be
//! that store. Two things can: a shared store on a third host, and the
//! Kubernetes API (resourceVersion CAS — the file tier's arbiter, see
//! `replica_sync.rs`).
//!
//! # What it carries, and what it must not
//!
//! It carries the facts it takes to decide WHO SERVES: the seat
//! `[epoch, composer]`, the per-leg sync marks, the serving lease, and
//! the target registry that turns a name into an address. Small,
//! identity-shaped, and read by MDS shards only — kernel clients never
//! see it.
//!
//! It does NOT carry the allocation lane: extents, geometry, layouts,
//! quarantine, admissions, delivered marks. Those stay in the volume's
//! home shard, transactional with each other, and — this is the half of
//! the invariant that survives — **the fence ENFORCEMENT path stays
//! witness-free**: minting a fence, preempting reservations and marking
//! them delivered are acts against the local record and the local tgt,
//! so a client can be fenced with the witness unreachable.
//!
//! # The one obligation the model pins on the implementation
//!
//! `FlintCompositionWitness.cfg` is green over a symmetric partition
//! only because the witness SERIALIZES the two racing writes: under a
//! cut cable the composer races to mark its peer stale while the peer
//! races to CAS the seat, and whoever lands second is refused (the mark
//! by the moved seat, the CAS by `ElectInSync` reading the fresh mark).
//! That is real **only if seat, marks and lease live in ONE
//! compare-and-swapped object per volume.** Split them across objects
//! and the race between them re-opens, with no run in the gate covering
//! that world. The sqlite implementation gets this from one transaction;
//! a Kubernetes implementation must get it from ONE resource's
//! resourceVersion, never from two.
//!
//! # The bill
//!
//! `FlintCompositionProbeBill.cfg` requires TLC to produce the state
//! where a perfectly healthy composer is suspended because only its path
//! to the witness failed: a composer that cannot prove its entitlement
//! must stop claiming it, or `WitnessDeadman`'s split brain is the
//! alternative. That is why [`WitnessError`] distinguishes `Unreachable`
//! from `Refused` at the type: a refusal is an ANSWER (the record moved,
//! the gate said no) and a cut is the absence of one, and the two must
//! never be collapsed — the same discipline as `Reachability` in
//! `block_export`, which names REACH and never liveness.

use std::sync::Arc;

use crate::state_backend::extent_alloc::{BlockLease, BlockLeg, BlockSeat, BlockTargetRow};

/// What went wrong talking to the witness — and the distinction is the
/// point, not an implementation detail.
///
/// * `Refused` — the witness ANSWERED and said no. The CAS raced, the
///   election gate refused a stale leg, a lease renewal was refused
///   because the record no longer names the caller. These are the
///   model's guards firing, and the caller must obey them.
/// * `Unreachable` — there was no answer. The model's `apiCut`: a
///   target that cannot reach the witness cannot promote, cannot
///   assemble, and cannot renew — and its lease therefore lapses and
///   the dead-man suspends it, which is the availability price the
///   arbiter decision signed up for.
///
/// Collapsing these is how a cut becomes a false refusal (or worse, a
/// refusal becomes a retryable blip).
#[derive(Debug)]
pub enum WitnessError {
    Refused(crate::state_backend::extent_alloc::ExtentAllocError),
    Unreachable(String),
}

impl std::fmt::Display for WitnessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(e) => write!(f, "{e}"),
            Self::Unreachable(m) => write!(f, "witness unreachable: {m}"),
        }
    }
}

impl WitnessError {
    /// True when the witness gave no answer. Callers use this to choose
    /// between OBEYING (a refusal) and DEFERRING (a cut) — a converge
    /// pass that treats a cut as a refusal would act on a record it
    /// never read.
    pub fn is_unreachable(&self) -> bool {
        matches!(self, Self::Unreachable(_))
    }

    /// The refusal underneath, when there is one.
    pub fn refusal(&self) -> Option<&crate::state_backend::extent_alloc::ExtentAllocError> {
        match self {
            Self::Refused(e) => Some(e),
            Self::Unreachable(_) => None,
        }
    }
}

pub type WitnessResult<T> = std::result::Result<T, WitnessError>;

/// The arbitration surface. Every method is a read or a
/// compare-and-swap against the one object that decides who serves a
/// volume.
#[async_trait::async_trait]
pub trait CompositionWitness: Send + Sync {
    /// A target announces where it can be dialled. Level-triggered:
    /// called every reconcile pass, so an address change converges
    /// without an operator — and, unlike the shard-local registry this
    /// replaces, a PEER's row is visible here, which is the first of
    /// the two facts a survivor's election was missing.
    async fn target_register(
        &self,
        target_id: &str,
        traddr: &str,
        trsvcid: u16,
        now_unix: i64,
    ) -> WitnessResult<()>;

    /// Every registered target: what the prober's subject list is built
    /// from. A target absent here cannot be probed, and a target that
    /// cannot be probed can never be condemned.
    async fn target_list(&self) -> WitnessResult<Vec<BlockTargetRow>>;

    /// Seat a volume at `composer` if it has no seat, and return the
    /// seat that stands either way — which the caller MUST compare,
    /// because a seat naming somebody else is never silently
    /// overwritten (`RecordAssemblyOnly`: seating can never be
    /// adoption). The first composition is also its first assembly, so
    /// this grants the epoch-1 lease and marks the composer's leg
    /// in-sync in the SAME transaction.
    async fn seat_volume(
        &self,
        volume: &str,
        composer: &str,
        now_unix: i64,
        lease_expires_unix: i64,
    ) -> WitnessResult<BlockSeat>;

    /// Who composes this volume — WHO, never WHERE. The converge guard
    /// wants exactly this: a reconciler can only configure the tgt on
    /// its own socket.
    async fn volume_seat(&self, volume: &str) -> WitnessResult<Option<BlockSeat>>;

    /// Every seat, for the pass's subject list and the startup audit.
    async fn seat_list(&self) -> WitnessResult<Vec<BlockSeat>>;

    /// volume → seat → dialable coordinates, in one read. Unseated and
    /// unknown-composer are distinct REFUSALS, never invitations to
    /// fall back on a configured address.
    async fn resolve_target(&self, volume: &str) -> WitnessResult<(BlockSeat, BlockTargetRow)>;

    /// THE PROMOTION CAS. Moves the seat to `candidate` iff it still
    /// reads what the caller saw, the candidate is registered, and its
    /// leg is in sync; the epoch advances by exactly one. This is the
    /// act the whole witness exists for — and the act whose serialization
    /// against a concurrent `leg_mark` is the module's implementation
    /// obligation.
    async fn promote(
        &self,
        volume: &str,
        expected_epoch: i64,
        expected_composer: &str,
        candidate: &str,
        now_unix: i64,
    ) -> WitnessResult<BlockSeat>;

    /// The volume's legs and their sync marks — the election gate's
    /// input, and the second fact a survivor was missing: the mark is
    /// EARNED by a rebuild the composer runs, and it has to be readable
    /// by the node that will one day be elected on it.
    async fn legs(&self, volume: &str) -> WitnessResult<Vec<BlockLeg>>;

    /// Move a leg's sync mark. The degrade barrier writes STALE here
    /// BEFORE it may ack a solo write, and the rebuild writes INSYNC
    /// here only after the copy lands — mark-then-degrade and
    /// member-then-mark, both stating the one rule twice: the record's
    /// optimism trails reality.
    async fn leg_mark(
        &self,
        volume: &str,
        target_id: &str,
        sync_state: &str,
        now_unix: i64,
    ) -> WitnessResult<()>;

    /// The standing serving lease — where the eviction horizon is read.
    async fn lease(&self, volume: &str) -> WitnessResult<Option<BlockLease>>;

    /// Grant the epoch's serving lease: ASSEMBLY's act, and the only
    /// way a lease comes into being (assembly IS the grant — tranche
    /// 3's finding, in one call).
    async fn lease_grant(
        &self,
        volume: &str,
        epoch: i64,
        holder: &str,
        expires_unix: i64,
    ) -> WitnessResult<BlockLease>;

    /// Extend a standing lease, RECORD-CONDITIONED: refused for a
    /// holder the seat no longer names (however healthy that holder is)
    /// and for one the seat names but assembly has not yet granted.
    /// Under a symmetric partition this refusal is the ONLY way a
    /// deposed composer learns it was deposed — which is why the
    /// dead-man's "certainty" is mechanism here, not axiom.
    async fn lease_renew(
        &self,
        volume: &str,
        holder: &str,
        expires_unix: i64,
    ) -> WitnessResult<BlockLease>;

    /// Every lease this target holds: the dead-man's work list.
    async fn leases_held(&self, holder: &str) -> WitnessResult<Vec<BlockLease>>;

    /// Surrender a lease.
    async fn lease_drop(&self, volume: &str) -> WitnessResult<bool>;

    /// Sweep a volume's whole arbitration record — DeleteVolume's act.
    async fn drop_volume(&self, volume: &str) -> WitnessResult<()>;
}

/// The witness backed by a [`StateBackend`](crate::state_backend::StateBackend).
///
/// Two very different deployments use this one adapter, and which one
/// you get depends ENTIRELY on which backend it wraps:
///
/// * **the shard's own sqlite** — the pre-witness world, bit-identical
///   to what shipped before this module existed. Correct for a
///   single-target volume (`replicas: 1`), where "both targets" is one
///   target and its own record trivially satisfies the witness
///   property. This is the default, so nothing changes for anybody who
///   has not asked for a second copy.
///
/// * **a sqlite file shared by every shard on a host** — a REAL witness
///   for the lima rig and kind: separate MDS processes, separate
///   volume records, one arbitration record, with sqlite's own
///   transactions supplying the CAS. This is what makes the two-target
///   rig able to fail over at zero cluster spend, and what keeps the
///   unit suite meaningful (an API server exists in neither).
///
/// A Kubernetes implementation — the production witness, one object per
/// volume under resourceVersion CAS — implements the same trait, and
/// everything above it is written against the trait alone.
pub struct BackendWitness {
    backend: Arc<dyn crate::state_backend::StateBackend>,
}

impl BackendWitness {
    pub fn new(backend: Arc<dyn crate::state_backend::StateBackend>) -> Self {
        Self { backend }
    }
}

/// Flatten the backend's nested result into the witness's one error.
///
/// The nesting is exactly the distinction the witness needs: the OUTER
/// error is the store failing to answer (`Unreachable` — for sqlite, a
/// locked or vanished file; for the shared-file rig, the case that
/// matters), and the INNER one is the record answering NO.
fn flatten<T>(
    r: crate::state_backend::StateBackendResult<
        Result<T, crate::state_backend::extent_alloc::ExtentAllocError>,
    >,
) -> WitnessResult<T> {
    match r {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(WitnessError::Refused(e)),
        Err(e) => Err(WitnessError::Unreachable(e.to_string())),
    }
}

#[async_trait::async_trait]
impl CompositionWitness for BackendWitness {
    async fn target_register(
        &self,
        target_id: &str,
        traddr: &str,
        trsvcid: u16,
        now_unix: i64,
    ) -> WitnessResult<()> {
        flatten(
            self.backend
                .block_target_register(target_id, traddr, trsvcid, now_unix)
                .await,
        )
    }

    async fn target_list(&self) -> WitnessResult<Vec<BlockTargetRow>> {
        flatten(self.backend.block_target_list().await)
    }

    async fn seat_volume(
        &self,
        volume: &str,
        composer: &str,
        now_unix: i64,
        lease_expires_unix: i64,
    ) -> WitnessResult<BlockSeat> {
        flatten(
            self.backend
                .block_seat_volume(volume, composer, now_unix, lease_expires_unix)
                .await,
        )
    }

    async fn volume_seat(&self, volume: &str) -> WitnessResult<Option<BlockSeat>> {
        flatten(self.backend.block_volume_seat(volume).await)
    }

    async fn seat_list(&self) -> WitnessResult<Vec<BlockSeat>> {
        flatten(self.backend.block_seat_list().await)
    }

    async fn resolve_target(&self, volume: &str) -> WitnessResult<(BlockSeat, BlockTargetRow)> {
        flatten(self.backend.block_resolve_target(volume).await)
    }

    async fn promote(
        &self,
        volume: &str,
        expected_epoch: i64,
        expected_composer: &str,
        candidate: &str,
        now_unix: i64,
    ) -> WitnessResult<BlockSeat> {
        flatten(
            self.backend
                .block_promote(
                    volume,
                    expected_epoch,
                    expected_composer,
                    candidate,
                    now_unix,
                )
                .await,
        )
    }

    async fn legs(&self, volume: &str) -> WitnessResult<Vec<BlockLeg>> {
        flatten(self.backend.block_legs(volume).await)
    }

    async fn leg_mark(
        &self,
        volume: &str,
        target_id: &str,
        sync_state: &str,
        now_unix: i64,
    ) -> WitnessResult<()> {
        flatten(
            self.backend
                .block_leg_mark(volume, target_id, sync_state, now_unix)
                .await,
        )
    }

    async fn lease(&self, volume: &str) -> WitnessResult<Option<BlockLease>> {
        flatten(self.backend.block_lease(volume).await)
    }

    async fn lease_grant(
        &self,
        volume: &str,
        epoch: i64,
        holder: &str,
        expires_unix: i64,
    ) -> WitnessResult<BlockLease> {
        flatten(
            self.backend
                .block_lease_grant(volume, epoch, holder, expires_unix)
                .await,
        )
    }

    async fn lease_renew(
        &self,
        volume: &str,
        holder: &str,
        expires_unix: i64,
    ) -> WitnessResult<BlockLease> {
        flatten(
            self.backend
                .block_lease_renew(volume, holder, expires_unix)
                .await,
        )
    }

    async fn leases_held(&self, holder: &str) -> WitnessResult<Vec<BlockLease>> {
        flatten(self.backend.block_leases_held(holder).await)
    }

    async fn lease_drop(&self, volume: &str) -> WitnessResult<bool> {
        flatten(self.backend.block_lease_drop(volume).await)
    }

    async fn drop_volume(&self, volume: &str) -> WitnessResult<()> {
        flatten(self.backend.extent_drop_volume(volume).await).map(|_rows| ())
    }
}
