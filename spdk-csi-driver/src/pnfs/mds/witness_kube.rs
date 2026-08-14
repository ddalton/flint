//! THE KUBERNETES WITNESS — the production arbiter for a replicated
//! block volume, and the third point of view two targets cannot supply
//! each other.
//!
//! `witness.rs` says why a witness must exist and what it carries. This
//! is the implementation that makes it real off one host: the seat, the
//! leg sync marks, the serving lease and the allow-list identities live
//! in a Kubernetes object, advanced by resourceVersion compare-and-swap
//! — the same arbiter the file tier already trusts for its own replica
//! record (`replica_sync.rs`), reached the same way.
//!
//! # ONE OBJECT PER VOLUME, and it is the model's obligation
//!
//! `FlintCompositionWitness.cfg` is green over a symmetric partition
//! only because the witness SERIALIZES the two racing writes: under a
//! cut cable the composer races to mark its peer stale while the peer
//! races to CAS the seat, and whoever lands second is REFUSED — the
//! mark by the moved seat, the CAS by `ElectInSync` reading the fresh
//! mark. Split those facts across two objects and each write succeeds
//! against its own resourceVersion, the race re-opens, and no run in
//! the gate covers that world.
//!
//! So a volume's whole arbitration record is ONE ConfigMap holding ONE
//! JSON document, and every mutation is read-modify-write under that
//! object's resourceVersion. `a_promote_racing_a_leg_mark_is_serialized`
//! is that obligation as a test, driven through a store that injects a
//! concurrent writer between the read and the write.
//!
//! # Why ConfigMaps and not a CRD
//!
//! A CRD would be the prettier type, and it would need installing,
//! versioning and an upgrade path before a single volume could fail
//! over. The witness needs a compare-and-swapped box of small facts,
//! which every cluster already has. The cost is that the document is
//! opaque to `kubectl get -o custom-columns`; `kubectl get cm -l
//! flint.io/kind=composition -o yaml` is the operator surface, and the
//! JSON inside is deliberately readable.
//!
//! The TARGET REGISTRY is a separate object PER TARGET, and that is not
//! a violation of the rule above: coordinates are not part of any race
//! (a target only ever writes its own row, every pass), and one shared
//! registry object would make every self-registration contend with
//! every other for no reason.
//!
//! # What it costs, stated plainly
//!
//! Every arbitration act is now an API call, so an API-server outage
//! longer than the lease TTL suspends REPLICATED volumes: renewals
//! cannot land, the dead-man fires, writes stall, nothing corrupts.
//! That is `FlintCompositionProbeBill.cfg`'s state, and the tranche
//! makes TLC produce it rather than leaving it as prose. Volumes with
//! `replicas: 1` never touch this file at all — they arbitrate in their
//! own shard's record, where "both targets" is one target.
//!
//! What does NOT become an API call is the fence ENFORCEMENT lane: the
//! preempt RPC, the delivered mark, the extent rows and the quarantine
//! sweep are acts against the local record and the local tgt. A client
//! can still be fenced with the control plane unreachable — see the
//! commit that moved the identities and left the enforcement behind.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::pnfs::mds::witness::{CompositionWitness, WitnessError, WitnessResult};
use crate::state_backend::extent_alloc::{
    BlockLease, BlockLeg, BlockSeat, BlockTargetRow, ExtentAllocError, LEG_INSYNC, LEG_STALE,
};

/// How many times a read-modify-write re-reads after losing the CAS.
/// Bounded on purpose: a caller that cannot land its write in this many
/// attempts is contending with something that is not going away, and
/// saying so beats spinning inside a reconcile pass that holds a lease.
const MAX_CAS_ATTEMPTS: usize = 5;

// ── the document ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Seat {
    pub epoch: i64,
    pub composer: String,
    pub seated_unix: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Leg {
    pub sync_state: String,
    pub marked_unix: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Lease {
    pub epoch: i64,
    pub holder: String,
    pub expires_unix: i64,
}

/// A volume's whole arbitration record — the box the CAS is about.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CompositionDoc {
    /// The volume this record is about. Written into the document and
    /// not merely into the object's name, because the name is a
    /// SANITIZED derivation (DNS-1123 plus a hash) and cannot be
    /// inverted — and every list operation here has to attribute its
    /// rows to a volume.
    #[serde(default)]
    pub volume: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub seat: Option<Seat>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    pub legs: BTreeMap<String, Leg>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub lease: Option<Lease>,
    /// client_id → host NQN: admissions earned at LAYOUTGET.
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    pub hosts: BTreeMap<String, String>,
    /// host NQN → node name: admissions earned at ControllerPublish.
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    pub attaches: BTreeMap<String, String>,
    /// client_id → host NQN: standing fences. The identities only; the
    /// enforcement lane is not here and must not be.
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    pub fences: BTreeMap<String, String>,
}

impl CompositionDoc {
    /// The allow-list: client-earned ∪ node-attached, distinct, sorted.
    /// Fenced clients are absent because the fence EVICTS their rows —
    /// the same shape sqlite has, where a fence's effect on the door is
    /// a deletion and never a subtraction at read time.
    fn allow_list(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .hosts
            .values()
            .cloned()
            .chain(self.attaches.keys().cloned())
            .collect();
        v.sort();
        v.dedup();
        v
    }
}

/// A target's dial coordinates. Its own object, written only by itself.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TargetDoc {
    pub target_id: String,
    pub traddr: String,
    pub trsvcid: u16,
    /// FIRST registration, kept across re-registrations: a target that
    /// comes back on a new address is the SAME target.
    pub registered_unix: i64,
    /// When this row was last WRITTEN — which is not a heartbeat and
    /// must never be read as one. An unchanged row is not rewritten
    /// (every target re-asserting every pass would be API traffic for
    /// nothing), and liveness on this tier is the prober's data-path
    /// verdict, never a timestamp.
    #[serde(default)]
    pub updated_unix: i64,
}

// ── the store ────────────────────────────────────────────────────────

/// What a witness needs from Kubernetes, and nothing more: fetch a
/// document with its version, write it back only if that version still
/// stands, list a kind, delete one.
///
/// It is a trait so the SEMANTICS above can be tested without an API
/// server — including the conflict path, which is the part that carries
/// the model's obligation and the part an integration test would be
/// least able to produce on demand.
#[async_trait::async_trait]
pub trait DocStore: Send + Sync {
    async fn get(&self, name: &str) -> WitnessResult<Option<(String, String)>>;
    /// `rv = None` means CREATE (fails with Conflict if it exists).
    async fn put(&self, name: &str, body: &str, rv: Option<&str>) -> WitnessResult<PutOutcome>;
    async fn list(&self, kind: &str) -> WitnessResult<Vec<String>>;
    async fn delete(&self, name: &str) -> WitnessResult<()>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PutOutcome {
    Wrote,
    /// Somebody else wrote first. NOT an error: it is the CAS doing its
    /// job, and the caller re-reads and re-decides on the fresh record —
    /// which is exactly how a refusal gets a chance to fire.
    Conflict,
}

pub const KIND_COMPOSITION: &str = "composition";
pub const KIND_TARGET: &str = "target";

/// The ONE place a kind becomes a name prefix. [`object_name`] writes
/// it and [`kind_of_name`] reads it back, both from here — because the
/// label a document is stored under and the selector `list` fetches it
/// by have to agree, and a rule stated twice is a rule that can drift.
///
/// It did drift: the store used to decide the label by asking whether
/// `-c-` appeared ANYWHERE in the name, so a TARGET whose id carried
/// that infix — `mds-c-2`, `gke-c-1-pool` — was labelled a composition
/// and disappeared from `target_list`, taking its dial coordinates with
/// it. A target that cannot be listed cannot be probed, placed against,
/// or promoted to. The in-memory store in the tests had the prefix rule
/// right, which is exactly why no unit test could see it.
fn name_prefix(kind: &str) -> String {
    format!("flint-blk-{}-", &kind[..1])
}

/// Which kind an object name belongs to — the inverse of the prefix,
/// and never a substring search.
pub fn kind_of_name(name: &str) -> &'static str {
    if name.starts_with(&name_prefix(KIND_COMPOSITION)) {
        KIND_COMPOSITION
    } else {
        KIND_TARGET
    }
}

/// Object names are derived, never chosen: DNS-1123, and a short hash
/// of the original so two ids that sanitize alike cannot collide.
pub fn object_name(kind: &str, id: &str) -> String {
    let mut safe: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    safe.truncate(180);
    let safe = safe.trim_matches(|c: char| c == '-' || c == '.').to_string();
    format!("{}{}-{:x}", name_prefix(kind), safe, fnv1a(id))
}

fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

// ── the witness ──────────────────────────────────────────────────────

pub struct KubeWitness {
    store: Arc<dyn DocStore>,
}

impl KubeWitness {
    pub fn new(store: Arc<dyn DocStore>) -> Self {
        Self { store }
    }

    async fn read_doc(&self, volume: &str) -> WitnessResult<(CompositionDoc, Option<String>)> {
        let name = object_name(KIND_COMPOSITION, volume);
        match self.store.get(&name).await? {
            Some((body, rv)) => {
                let doc: CompositionDoc = serde_json::from_str(&body).map_err(|e| {
                    // A document we cannot parse is NOT an empty one:
                    // treating it as empty would re-seat a live volume
                    // and mint a second composition. Refuse loudly.
                    refused(ExtentAllocError::Corruption(format!(
                        "composition record for '{volume}' is unreadable ({e}) — refusing to \
                         treat it as absent"
                    )))
                })?;
                Ok((doc, Some(rv)))
            }
            None => Ok((CompositionDoc::default(), None)),
        }
    }

    /// THE COMPARE-AND-SWAP, and every mutation in this file goes
    /// through it: read the whole record, let `f` decide against what
    /// it actually says, write back only if nothing moved underneath.
    /// A conflict re-reads and re-decides — the decision is never
    /// carried over, because the fact it was made against may be the
    /// one that changed.
    async fn mutate<T, F>(&self, volume: &str, mut f: F) -> WitnessResult<T>
    where
        F: FnMut(&mut CompositionDoc) -> Result<T, WitnessError>,
    {
        let name = object_name(KIND_COMPOSITION, volume);
        for _ in 0..MAX_CAS_ATTEMPTS {
            let (mut doc, rv) = self.read_doc(volume).await?;
            doc.volume = volume.to_string();
            let before = doc.clone();
            let out = f(&mut doc)?;
            if doc == before {
                // A no-op needs no write, and writing anyway would make
                // every reconcile pass contend for nothing.
                return Ok(out);
            }
            let body = serde_json::to_string(&doc).map_err(|e| {
                refused(ExtentAllocError::Corruption(format!("encoding: {e}")))
            })?;
            match self.store.put(&name, &body, rv.as_deref()).await? {
                PutOutcome::Wrote => return Ok(out),
                PutOutcome::Conflict => continue,
            }
        }
        Err(WitnessError::Unreachable(format!(
            "the composition record for '{volume}' did not settle in {MAX_CAS_ATTEMPTS} attempts \
             — something else is writing it continuously"
        )))
    }
}

/// Refusals reuse the RECORD'S OWN error vocabulary rather than a
/// string: `attempt_promotion` matches on `PromotionRaced`, the attach
/// lane matches on `FencedClient`, and a witness that invented its own
/// types would make every one of those matches fall through to a
/// generic message. The sqlite backend and this one refuse in the same
/// words because they are refusing the same things.
fn refused(e: ExtentAllocError) -> WitnessError {
    WitnessError::Refused(e)
}

#[async_trait::async_trait]
impl CompositionWitness for KubeWitness {
    async fn target_register(
        &self,
        target_id: &str,
        traddr: &str,
        trsvcid: u16,
        now_unix: i64,
    ) -> WitnessResult<()> {
        let name = object_name(KIND_TARGET, target_id);
        let existing = self.store.get(&name).await?;
        let (rv, keep_registered) = match &existing {
            Some((body, rv)) => {
                let prev: TargetDoc = serde_json::from_str(body).unwrap_or_default();
                // A target that comes back on a new address is the SAME
                // target: its first registration timestamp is kept, and
                // the distinction between "new" and "moved" stays
                // readable.
                if prev.traddr == traddr && prev.trsvcid == trsvcid {
                    return Ok(());
                }
                (Some(rv.clone()), prev.registered_unix)
            }
            None => (None, now_unix),
        };
        let doc = TargetDoc {
            target_id: target_id.into(),
            traddr: traddr.into(),
            trsvcid,
            registered_unix: keep_registered,
            updated_unix: now_unix,
        };
        let body = serde_json::to_string(&doc)
            .map_err(|e| refused(ExtentAllocError::Corruption(format!("encoding target: {e}"))))?;
        match self.store.put(&name, &body, rv.as_deref()).await? {
            // A lost race here is another writer registering the same
            // target with the same facts; the next pass re-asserts.
            PutOutcome::Wrote | PutOutcome::Conflict => Ok(()),
        }
    }

    async fn target_list(&self) -> WitnessResult<Vec<BlockTargetRow>> {
        let mut out = Vec::new();
        for body in self.store.list(KIND_TARGET).await? {
            if let Ok(d) = serde_json::from_str::<TargetDoc>(&body) {
                out.push(BlockTargetRow {
                    target_id: d.target_id,
                    traddr: d.traddr,
                    trsvcid: d.trsvcid,
                    registered_unix: d.registered_unix,
                    updated_unix: d.updated_unix,
                });
            }
        }
        out.sort_by(|a, b| a.target_id.cmp(&b.target_id));
        Ok(out)
    }

    async fn seat_volume(
        &self,
        volume: &str,
        composer: &str,
        now_unix: i64,
        lease_expires_unix: i64,
    ) -> WitnessResult<BlockSeat> {
        let v = volume.to_string();
        let c = composer.to_string();
        self.mutate(volume, move |doc| {
            // INSERT-IF-ABSENT, and this is the whole of
            // `RecordAssemblyOnly` at provisioning time: seating can
            // never be adoption, so a seat that already names somebody
            // else is returned unchanged for the caller to compare.
            if doc.seat.is_none() {
                doc.seat = Some(Seat { epoch: 1, composer: c.clone(), seated_unix: now_unix });
                doc.legs.entry(c.clone()).or_insert(Leg {
                    sync_state: LEG_INSYNC.into(),
                    marked_unix: now_unix,
                });
                // The first composition IS an assembly, so it is also
                // the first lease grant — one act.
                doc.lease = Some(Lease {
                    epoch: 1,
                    holder: c.clone(),
                    expires_unix: lease_expires_unix,
                });
            }
            let s = doc.seat.clone().expect("just set");
            Ok(BlockSeat {
                volume: v.clone(),
                epoch: s.epoch,
                composer: s.composer,
                seated_unix: s.seated_unix,
            })
        })
        .await
    }

    async fn volume_seat(&self, volume: &str) -> WitnessResult<Option<BlockSeat>> {
        let (doc, _) = self.read_doc(volume).await?;
        Ok(doc.seat.map(|s| BlockSeat {
            volume: volume.into(),
            epoch: s.epoch,
            composer: s.composer,
            seated_unix: s.seated_unix,
        }))
    }

    async fn seat_list(&self) -> WitnessResult<Vec<BlockSeat>> {
        let mut out = Vec::new();
        for body in self.store.list(KIND_COMPOSITION).await? {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) else {
                continue;
            };
            let Some(volume) = v.get("volume").and_then(|x| x.as_str()) else {
                continue;
            };
            if let Ok(doc) = serde_json::from_str::<CompositionDoc>(&body) {
                if let Some(s) = doc.seat {
                    out.push(BlockSeat {
                        volume: volume.into(),
                        epoch: s.epoch,
                        composer: s.composer,
                        seated_unix: s.seated_unix,
                    });
                }
            }
        }
        out.sort_by(|a, b| a.volume.cmp(&b.volume));
        Ok(out)
    }

    async fn resolve_target(&self, volume: &str) -> WitnessResult<(BlockSeat, BlockTargetRow)> {
        let seat = self
            .volume_seat(volume)
            .await?
            .ok_or_else(|| refused(ExtentAllocError::UnseatedVolume))?;
        let targets = self.target_list().await?;
        let row = targets
            .into_iter()
            .find(|t| t.target_id == seat.composer)
            .ok_or_else(|| {
                refused(ExtentAllocError::UnknownComposer { composer: seat.composer.clone() })
            })?;
        Ok((seat, row))
    }

    async fn promote(
        &self,
        volume: &str,
        expected_epoch: i64,
        expected_composer: &str,
        candidate: &str,
        now_unix: i64,
    ) -> WitnessResult<BlockSeat> {
        // Read BEFORE the CAS, and outside it: "affirmatively heard
        // from" is a fact about the registry, which is not part of this
        // object's race. A stale read here can only refuse.
        let registered = self.target_list().await?;
        let known = registered.iter().any(|t| t.target_id == candidate);
        let v = volume.to_string();
        let expected_composer = expected_composer.to_string();
        let candidate = candidate.to_string();
        self.mutate(volume, move |doc| {
            let seat = doc
                .seat
                .clone()
                .ok_or_else(|| refused(ExtentAllocError::UnseatedVolume))?;
            if seat.epoch != expected_epoch || seat.composer != expected_composer {
                return Err(WitnessError::Refused(ExtentAllocError::PromotionRaced {
                    epoch: seat.epoch,
                    composer: seat.composer.clone(),
                }));
            }
            if seat.composer == candidate {
                return Err(refused(ExtentAllocError::SelfPromotion {
                    composer: candidate.clone(),
                }));
            }
            if !known {
                return Err(refused(ExtentAllocError::UnknownComposer {
                    composer: candidate.clone(),
                }));
            }
            // ElectInSync: only a leg the record carries as in-sync.
            match doc.legs.get(&candidate) {
                Some(l) if l.sync_state == LEG_INSYNC => {}
                _ => {
                    return Err(refused(ExtentAllocError::NotInSync {
                        candidate: candidate.clone(),
                    }))
                }
            }
            // The CAS does NOT mark the deposed leg stale: between here
            // and assembly the deposed composer may still be acking, and
            // demoting it now is the loss this gate exists to prevent.
            doc.seat = Some(Seat {
                epoch: seat.epoch + 1,
                composer: candidate.clone(),
                seated_unix: now_unix,
            });
            let s = doc.seat.clone().expect("just set");
            Ok(BlockSeat {
                volume: v.clone(),
                epoch: s.epoch,
                composer: s.composer,
                seated_unix: s.seated_unix,
            })
        })
        .await
    }

    async fn legs(&self, volume: &str) -> WitnessResult<Vec<BlockLeg>> {
        let (doc, _) = self.read_doc(volume).await?;
        let mut out: Vec<BlockLeg> = doc
            .legs
            .into_iter()
            .map(|(target_id, l)| BlockLeg {
                volume: volume.into(),
                target_id,
                sync_state: l.sync_state,
                marked_unix: l.marked_unix,
            })
            .collect();
        out.sort_by(|a, b| a.target_id.cmp(&b.target_id));
        Ok(out)
    }

    async fn leg_mark(
        &self,
        volume: &str,
        target_id: &str,
        sync_state: &str,
        now_unix: i64,
    ) -> WitnessResult<()> {
        if sync_state != LEG_INSYNC && sync_state != LEG_STALE {
            return Err(refused(ExtentAllocError::InvalidRange("leg sync state")));
        }
        let target_id = target_id.to_string();
        let sync_state = sync_state.to_string();
        self.mutate(volume, move |doc| {
            doc.legs.insert(
                target_id.clone(),
                Leg { sync_state: sync_state.clone(), marked_unix: now_unix },
            );
            Ok(())
        })
        .await
    }

    async fn lease(&self, volume: &str) -> WitnessResult<Option<BlockLease>> {
        let (doc, _) = self.read_doc(volume).await?;
        Ok(doc.lease.map(|l| BlockLease {
            volume: volume.into(),
            epoch: l.epoch,
            holder: l.holder,
            expires_unix: l.expires_unix,
        }))
    }

    async fn lease_grant(
        &self,
        volume: &str,
        epoch: i64,
        holder: &str,
        expires_unix: i64,
    ) -> WitnessResult<BlockLease> {
        let v = volume.to_string();
        let holder = holder.to_string();
        self.mutate(volume, move |doc| {
            doc.lease = Some(Lease { epoch, holder: holder.clone(), expires_unix });
            Ok(BlockLease {
                volume: v.clone(),
                epoch,
                holder: holder.clone(),
                expires_unix,
            })
        })
        .await
    }

    async fn lease_renew(
        &self,
        volume: &str,
        holder: &str,
        expires_unix: i64,
    ) -> WitnessResult<BlockLease> {
        let v = volume.to_string();
        let holder = holder.to_string();
        self.mutate(volume, move |doc| {
            let seat = doc
                .seat
                .clone()
                .ok_or_else(|| refused(ExtentAllocError::UnseatedVolume))?;
            // RECORD-CONDITIONED, both halves — tranche 3's finding.
            // (a) a deposed holder is refused however healthy it is, or
            // the eviction horizon never comes;
            if seat.composer != holder {
                return Err(refused(ExtentAllocError::LeaseRefused {
                    reason: format!(
                        "'{holder}' is not the composer of '{v}' — the record seats it at '{}' \
                         (epoch {})",
                        seat.composer, seat.epoch
                    ),
                }));
            }
            // (b) an ELECTED holder is refused too: assembly grants a
            // lease, a holder never takes one.
            let lease = doc.lease.clone().ok_or_else(|| {
                refused(ExtentAllocError::LeaseRefused {
                    reason: format!(
                        "no serving lease stands on '{v}' — assembly grants it, the holder does \
                         not take it"
                    ),
                })
            })?;
            if lease.holder != holder || lease.epoch != seat.epoch {
                return Err(refused(ExtentAllocError::LeaseRefused {
                    reason: format!(
                        "the standing lease on '{v}' belongs to '{}' at epoch {}, not to \
                         '{holder}' at epoch {} — assembly grants it, the holder does not take it",
                        lease.holder, lease.epoch, seat.epoch
                    ),
                }));
            }
            doc.lease = Some(Lease { epoch: lease.epoch, holder: holder.clone(), expires_unix });
            Ok(BlockLease {
                volume: v.clone(),
                epoch: lease.epoch,
                holder: holder.clone(),
                expires_unix,
            })
        })
        .await
    }

    async fn leases_held(&self, holder: &str) -> WitnessResult<Vec<BlockLease>> {
        let mut out = Vec::new();
        for body in self.store.list(KIND_COMPOSITION).await? {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) else {
                continue;
            };
            let Some(volume) = v.get("volume").and_then(|x| x.as_str()) else {
                continue;
            };
            if let Ok(doc) = serde_json::from_str::<CompositionDoc>(&body) {
                if let Some(l) = doc.lease.filter(|l| l.holder == holder) {
                    out.push(BlockLease {
                        volume: volume.into(),
                        epoch: l.epoch,
                        holder: l.holder,
                        expires_unix: l.expires_unix,
                    });
                }
            }
        }
        out.sort_by(|a, b| a.volume.cmp(&b.volume));
        Ok(out)
    }

    async fn lease_drop(&self, volume: &str) -> WitnessResult<bool> {
        self.mutate(volume, |doc| Ok(doc.lease.take().is_some())).await
    }

    async fn drop_volume(&self, volume: &str) -> WitnessResult<()> {
        self.store.delete(&object_name(KIND_COMPOSITION, volume)).await
    }

    async fn hosts(&self, volume: &str) -> WitnessResult<Vec<String>> {
        let (doc, _) = self.read_doc(volume).await?;
        Ok(doc.allow_list())
    }

    async fn host_admit(
        &self,
        volume: &str,
        client_id: u64,
        host_nqn: &str,
        _now_unix: i64,
    ) -> WitnessResult<Vec<String>> {
        let host_nqn = host_nqn.to_string();
        self.mutate(volume, move |doc| {
            doc.hosts.insert(client_id.to_string(), host_nqn.clone());
            Ok(doc.allow_list())
        })
        .await
    }

    async fn host_evict(
        &self,
        volume: &str,
        client_id: u64,
    ) -> WitnessResult<(Vec<String>, Vec<String>)> {
        self.mutate(volume, move |doc| {
            let removed = doc.hosts.remove(&client_id.to_string());
            let mut evicted = Vec::new();
            if let Some(nqn) = removed {
                // The attach row goes with it: leaving it would keep a
                // fenced NQN on the allow-list through the side door the
                // fence rig found live (F5).
                doc.attaches.remove(&nqn);
                let still_desired = doc.hosts.values().any(|h| h == &nqn);
                if !still_desired {
                    evicted.push(nqn);
                }
            }
            Ok((evicted, doc.allow_list()))
        })
        .await
    }

    async fn node_attach(
        &self,
        volume: &str,
        host_nqn: &str,
        node_name: &str,
        _now_unix: i64,
    ) -> WitnessResult<Vec<String>> {
        let host_nqn = host_nqn.to_string();
        let node_name = node_name.to_string();
        self.mutate(volume, move |doc| {
            // The fence door, and it has to be shut HERE: a survivor
            // that never saw the fence being minted refuses the
            // re-attach because the identity travelled with the record.
            if doc.fences.values().any(|n| n == &host_nqn) {
                return Err(WitnessError::Refused(ExtentAllocError::FencedClient));
            }
            doc.attaches.insert(host_nqn.clone(), node_name.clone());
            Ok(doc.allow_list())
        })
        .await
    }

    async fn node_detach(
        &self,
        volume: &str,
        host_nqn: &str,
    ) -> WitnessResult<(bool, Vec<String>)> {
        let host_nqn = host_nqn.to_string();
        self.mutate(volume, move |doc| {
            let removed = doc.attaches.remove(&host_nqn).is_some();
            Ok((removed, doc.allow_list()))
        })
        .await
    }

    async fn fence_record(
        &self,
        volume: &str,
        client_id: u64,
        _now_unix: i64,
    ) -> WitnessResult<String> {
        self.mutate(volume, move |doc| {
            // Capture the NQN while the admission still holds it —
            // pre-eviction, exactly as sqlite does, or the fence records
            // an empty name and the survivor's door has nothing to keep
            // shut.
            let nqn = doc.hosts.get(&client_id.to_string()).cloned().unwrap_or_default();
            doc.fences.insert(client_id.to_string(), nqn.clone());
            Ok(nqn)
        })
        .await
    }

    async fn is_fenced(&self, volume: &str, client_id: u64) -> WitnessResult<bool> {
        let (doc, _) = self.read_doc(volume).await?;
        Ok(doc.fences.contains_key(&client_id.to_string()))
    }

    async fn fenced_all(&self) -> WitnessResult<Vec<(String, u64)>> {
        let mut out = Vec::new();
        for body in self.store.list(KIND_COMPOSITION).await? {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) else {
                continue;
            };
            let Some(volume) = v.get("volume").and_then(|x| x.as_str()) else {
                continue;
            };
            if let Ok(doc) = serde_json::from_str::<CompositionDoc>(&body) {
                for c in doc.fences.keys() {
                    if let Ok(id) = c.parse::<u64>() {
                        out.push((volume.to_string(), id));
                    }
                }
            }
        }
        out.sort();
        Ok(out)
    }

    async fn unfence(&self, volume: &str, client_id: u64) -> WitnessResult<bool> {
        self.mutate(volume, move |doc| Ok(doc.fences.remove(&client_id.to_string()).is_some()))
            .await
    }
}

// ── the Kubernetes store ─────────────────────────────────────────────

/// ConfigMaps in the driver's own namespace, one document per object,
/// CAS'd on resourceVersion — the same lever `replica_sync.rs` pulls on
/// a PV annotation, and for the same reason: the API server refuses a
/// write whose resourceVersion has moved, which is the only compare-
/// and-swap either tier needs.
pub struct ConfigMapStore {
    api: kube::Api<k8s_openapi::api::core::v1::ConfigMap>,
}

/// The key the document lives under, and the label that makes a kind
/// listable. Both are part of the operator surface — `kubectl get cm -l
/// flint.io/kind=composition -o yaml` is how a human reads the record.
const DATA_KEY: &str = "record.json";
const KIND_LABEL: &str = "flint.io/kind";

impl ConfigMapStore {
    pub async fn new(namespace: &str) -> WitnessResult<Self> {
        let client = kube::Client::try_default().await.map_err(|e| {
            // No client is UNREACHABLE, never a refusal: a witness we
            // cannot reach must make callers defer, not act.
            WitnessError::Unreachable(format!("kube client: {e}"))
        })?;
        Ok(Self { api: kube::Api::namespaced(client, namespace) })
    }
}

fn kube_err(e: kube::Error) -> WitnessError {
    WitnessError::Unreachable(format!("witness API: {e}"))
}

#[async_trait::async_trait]
impl DocStore for ConfigMapStore {
    async fn get(&self, name: &str) -> WitnessResult<Option<(String, String)>> {
        match self.api.get_opt(name).await.map_err(kube_err)? {
            Some(cm) => {
                let rv = cm.metadata.resource_version.clone().unwrap_or_default();
                let body = cm
                    .data
                    .as_ref()
                    .and_then(|d| d.get(DATA_KEY))
                    .cloned()
                    .unwrap_or_else(|| "{}".into());
                Ok(Some((body, rv)))
            }
            None => Ok(None),
        }
    }

    async fn put(&self, name: &str, body: &str, rv: Option<&str>) -> WitnessResult<PutOutcome> {
        let kind = kind_of_name(name);
        let mut meta = serde_json::json!({
            "name": name,
            "labels": {
                KIND_LABEL: kind,
                "app.kubernetes.io/managed-by": "flint",
            }
        });
        if let Some(rv) = rv {
            // THE COMPARE-AND-SWAP: the API server rejects this write
            // with 409 if anything moved since the read it was decided
            // against.
            meta["resourceVersion"] = serde_json::Value::String(rv.to_string());
        }
        let patch = serde_json::json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": meta,
            "data": { DATA_KEY: body },
        });
        let res = if rv.is_some() {
            self.api
                .patch(
                    name,
                    &kube::api::PatchParams::default(),
                    &kube::api::Patch::Merge(&patch),
                )
                .await
                .map(|_| ())
        } else {
            let cm: k8s_openapi::api::core::v1::ConfigMap =
                serde_json::from_value(patch).map_err(|e| {
                    refused(ExtentAllocError::Corruption(format!("building ConfigMap: {e}")))
                })?;
            self.api
                .create(&kube::api::PostParams::default(), &cm)
                .await
                .map(|_| ())
        };
        match res {
            Ok(()) => Ok(PutOutcome::Wrote),
            // 409 on a patch is a lost CAS; 409 on a create is somebody
            // else creating the same record first. Both mean the same
            // thing to the caller: re-read and decide again.
            Err(kube::Error::Api(ae)) if ae.code == 409 => Ok(PutOutcome::Conflict),
            Err(e) => Err(kube_err(e)),
        }
    }

    async fn list(&self, kind: &str) -> WitnessResult<Vec<String>> {
        let lp = kube::api::ListParams::default().labels(&format!("{KIND_LABEL}={kind}"));
        let list = self.api.list(&lp).await.map_err(kube_err)?;
        Ok(list
            .items
            .into_iter()
            .filter_map(|cm| cm.data.and_then(|d| d.get(DATA_KEY).cloned()))
            .collect())
    }

    async fn delete(&self, name: &str) -> WitnessResult<()> {
        match self.api.delete(name, &kube::api::DeleteParams::default()).await {
            Ok(_) => Ok(()),
            // Already gone is the desired state, not a failure.
            Err(kube::Error::Api(ae)) if ae.code == 404 => Ok(()),
            Err(e) => Err(kube_err(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// An in-memory store with real version semantics — and, more to the
    /// point, a way to make somebody else win the race: `interpose`
    /// runs ONCE before the next `put`, standing in for the peer whose
    /// write landed while ours was being decided. That is the only way
    /// to exercise the conflict path deliberately; against a real API
    /// server it is a matter of luck and timing.
    #[derive(Default)]
    struct MemStore {
        docs: Mutex<std::collections::HashMap<String, (String, u64)>>,
        interpose: Mutex<Option<Box<dyn FnOnce(&mut std::collections::HashMap<String, (String, u64)>) + Send>>>,
        puts: Mutex<usize>,
        conflicts: Mutex<usize>,
    }

    impl MemStore {
        fn arm(&self, f: impl FnOnce(&mut std::collections::HashMap<String, (String, u64)>) + Send + 'static) {
            *self.interpose.lock().unwrap() = Some(Box::new(f));
        }
    }

    #[async_trait::async_trait]
    impl DocStore for MemStore {
        async fn get(&self, name: &str) -> WitnessResult<Option<(String, String)>> {
            Ok(self
                .docs
                .lock()
                .unwrap()
                .get(name)
                .map(|(b, v)| (b.clone(), v.to_string())))
        }

        async fn put(&self, name: &str, body: &str, rv: Option<&str>) -> WitnessResult<PutOutcome> {
            let mut docs = self.docs.lock().unwrap();
            if let Some(f) = self.interpose.lock().unwrap().take() {
                f(&mut docs);
            }
            *self.puts.lock().unwrap() += 1;
            let cur = docs.get(name).map(|(_, v)| *v);
            let ok = match (cur, rv) {
                (None, None) => true,
                (Some(v), Some(want)) => want.parse::<u64>().ok() == Some(v),
                _ => false,
            };
            if !ok {
                *self.conflicts.lock().unwrap() += 1;
                return Ok(PutOutcome::Conflict);
            }
            let next = cur.unwrap_or(0) + 1;
            docs.insert(name.into(), (body.into(), next));
            Ok(PutOutcome::Wrote)
        }

        async fn list(&self, kind: &str) -> WitnessResult<Vec<String>> {
            // The same rule the real store labels by — a label selector
            // over what `put` wrote. A fake that classifies documents
            // its own way tests the fake.
            Ok(self
                .docs
                .lock()
                .unwrap()
                .iter()
                .filter(|(n, _)| kind_of_name(n) == kind)
                .map(|(_, (b, _))| b.clone())
                .collect())
        }

        async fn delete(&self, name: &str) -> WitnessResult<()> {
            self.docs.lock().unwrap().remove(name);
            Ok(())
        }
    }

    fn witness() -> (KubeWitness, Arc<MemStore>) {
        let store = Arc::new(MemStore::default());
        (KubeWitness::new(Arc::clone(&store) as Arc<dyn DocStore>), store)
    }

    /// THE MODEL'S OBLIGATION, and the reason a volume's whole
    /// arbitration record is ONE object.
    ///
    /// `FlintCompositionWitness.cfg` is green over a symmetric partition
    /// because the witness serializes the two racing writes: the peer
    /// CASes the seat while the composer marks that peer stale, and
    /// whoever lands SECOND is refused. Here the leg mark lands first,
    /// interposed between the promotion's read and its write — so the
    /// promotion re-reads, finds the mark it was elected on gone, and
    /// refuses with `ElectInSync` rather than writing a decision made
    /// against a record that no longer exists.
    ///
    /// Split seat and legs across two objects and each write succeeds
    /// against its own version: the peer becomes composer of a volume
    /// the composer has already recorded as missing acked writes.
    #[tokio::test]
    async fn a_promote_racing_a_leg_mark_is_serialized() {
        let (w, store) = witness();
        w.target_register("node-a", "10.0.0.1", 4420, 1).await.unwrap();
        w.target_register("node-b", "10.0.0.2", 4420, 1).await.unwrap();
        w.seat_volume("pvc-r", "node-a", 1, 100).await.unwrap();
        w.leg_mark("pvc-r", "node-b", LEG_INSYNC, 2).await.unwrap();

        // The composer's degrade lands between our read and our write:
        // node-b is stale as of now.
        let name = object_name(KIND_COMPOSITION, "pvc-r");
        store.arm(move |docs| {
            let (body, v) = docs.get(&name).cloned().unwrap();
            let mut doc: CompositionDoc = serde_json::from_str(&body).unwrap();
            doc.legs.insert(
                "node-b".into(),
                Leg { sync_state: LEG_STALE.into(), marked_unix: 3 },
            );
            docs.insert(name.clone(), (serde_json::to_string(&doc).unwrap(), v + 1));
        });

        let err = w
            .promote("pvc-r", 1, "node-a", "node-b", 4)
            .await
            .expect_err("the election must not stand on a mark that was withdrawn");
        assert!(
            matches!(err.refusal(), Some(ExtentAllocError::NotInSync { .. })),
            "refused for the right reason: {err}"
        );
        assert!(*store.conflicts.lock().unwrap() >= 1, "the CAS actually lost a round");
        // And the seat did NOT move.
        let seat = w.volume_seat("pvc-r").await.unwrap().unwrap();
        assert_eq!((seat.epoch, seat.composer.as_str()), (1, "node-a"));
    }

    /// The other order of the same race, and the other half of the
    /// serialization: the promotion lands first, so the composer's
    /// degrade — decided when it was still the composer — re-reads and
    /// finds the seat gone. Its mark is refused by the same object.
    #[tokio::test]
    async fn a_leg_mark_racing_a_promote_re_reads_the_moved_seat() {
        let (w, store) = witness();
        w.target_register("node-b", "10.0.0.2", 4420, 1).await.unwrap();
        w.seat_volume("pvc-o", "node-a", 1, 100).await.unwrap();
        w.leg_mark("pvc-o", "node-b", LEG_INSYNC, 2).await.unwrap();

        let name = object_name(KIND_COMPOSITION, "pvc-o");
        let n2 = name.clone();
        store.arm(move |docs| {
            let (body, v) = docs.get(&n2).cloned().unwrap();
            let mut doc: CompositionDoc = serde_json::from_str(&body).unwrap();
            doc.seat = Some(Seat { epoch: 2, composer: "node-b".into(), seated_unix: 3 });
            docs.insert(n2.clone(), (serde_json::to_string(&doc).unwrap(), v + 1));
        });

        // The deposed composer's renewal, decided before it knew.
        let err = w
            .lease_renew("pvc-o", "node-a", 200)
            .await
            .expect_err("a deposed holder must be refused however healthy it is");
        assert!(matches!(err.refusal(), Some(ExtentAllocError::LeaseRefused { .. })), "{err}");
        assert_eq!(w.volume_seat("pvc-o").await.unwrap().unwrap().epoch, 2);
    }

    /// Seating is INSERT-IF-ABSENT — `RecordAssemblyOnly` at
    /// provisioning time. A second seating returns the seat that
    /// stands, so the caller can compare and refuse; it never adopts.
    #[tokio::test]
    async fn seating_never_adopts_a_volume_somebody_else_composes() {
        let (w, _) = witness();
        let first = w.seat_volume("pvc-s", "node-a", 1, 100).await.unwrap();
        assert_eq!((first.epoch, first.composer.as_str()), (1, "node-a"));
        let second = w.seat_volume("pvc-s", "node-b", 2, 200).await.unwrap();
        assert_eq!(
            (second.epoch, second.composer.as_str()),
            (1, "node-a"),
            "the standing seat is returned unchanged"
        );
        // And the first seating granted the epoch-1 lease and marked its
        // own leg in sync — one act, because the first composition IS an
        // assembly.
        let lease = w.lease("pvc-s").await.unwrap().unwrap();
        assert_eq!((lease.epoch, lease.holder.as_str()), (1, "node-a"));
        let legs = w.legs("pvc-s").await.unwrap();
        assert_eq!(legs.len(), 1);
        assert_eq!((legs[0].target_id.as_str(), legs[0].sync_state.as_str()), ("node-a", LEG_INSYNC));
    }

    /// The election gates, each refusing for its own reason and in the
    /// record's own vocabulary — so `attempt_promotion`'s matches read
    /// the same against this witness as against sqlite.
    #[tokio::test]
    async fn the_election_gates_refuse_in_the_records_own_words() {
        let (w, _) = witness();
        w.seat_volume("pvc-g", "node-a", 1, 100).await.unwrap();

        // Unregistered candidate: electing into a black hole.
        w.leg_mark("pvc-g", "node-b", LEG_INSYNC, 2).await.unwrap();
        let e = w.promote("pvc-g", 1, "node-a", "node-b", 3).await.unwrap_err();
        assert!(matches!(e.refusal(), Some(ExtentAllocError::UnknownComposer { .. })), "{e}");

        // Registered but stale: ElectInSync.
        w.target_register("node-b", "10.0.0.2", 4420, 1).await.unwrap();
        w.leg_mark("pvc-g", "node-b", LEG_STALE, 4).await.unwrap();
        let e = w.promote("pvc-g", 1, "node-a", "node-b", 5).await.unwrap_err();
        assert!(matches!(e.refusal(), Some(ExtentAllocError::NotInSync { .. })), "{e}");

        // A stale expectation: somebody else already moved the seat.
        w.leg_mark("pvc-g", "node-b", LEG_INSYNC, 6).await.unwrap();
        let e = w.promote("pvc-g", 9, "node-a", "node-b", 7).await.unwrap_err();
        assert!(matches!(e.refusal(), Some(ExtentAllocError::PromotionRaced { .. })), "{e}");

        // The sitting composer is not a candidate.
        let e = w.promote("pvc-g", 1, "node-a", "node-a", 8).await.unwrap_err();
        assert!(matches!(e.refusal(), Some(ExtentAllocError::SelfPromotion { .. })), "{e}");

        // And with every gate satisfied, the epoch advances by exactly
        // one — and the deposed leg is NOT demoted here, because until
        // assembly it may still be acking.
        let seat = w.promote("pvc-g", 1, "node-a", "node-b", 9).await.unwrap();
        assert_eq!((seat.epoch, seat.composer.as_str()), (2, "node-b"));
        let legs = w.legs("pvc-g").await.unwrap();
        let a = legs.iter().find(|l| l.target_id == "node-a").unwrap();
        assert_eq!(a.sync_state, LEG_INSYNC, "the CAS does not demote the deposed leg");
    }

    /// The allow-list identities, and the door they build: a fenced
    /// client is off the list and its node cannot re-attach — which is
    /// the whole reason these facts are in the witness rather than in
    /// the shard that minted them.
    #[tokio::test]
    async fn the_door_travels_with_the_record() {
        let (w, _) = witness();
        w.seat_volume("pvc-d", "node-a", 1, 100).await.unwrap();
        w.node_attach("pvc-d", "nqn:node-live", "node-live", 1).await.unwrap();
        w.host_admit("pvc-d", 9, "nqn:node-doomed", 1).await.unwrap();
        assert_eq!(
            w.hosts("pvc-d").await.unwrap(),
            vec!["nqn:node-doomed".to_string(), "nqn:node-live".to_string()]
        );

        // The fence captures the NQN from the live admission, then the
        // eviction takes it off the door.
        assert_eq!(w.fence_record("pvc-d", 9, 2).await.unwrap(), "nqn:node-doomed");
        assert!(w.is_fenced("pvc-d", 9).await.unwrap());
        let (evicted, remaining) = w.host_evict("pvc-d", 9).await.unwrap();
        assert_eq!(evicted, vec!["nqn:node-doomed".to_string()]);
        assert_eq!(remaining, vec!["nqn:node-live".to_string()]);

        // A survivor that never saw the fence being minted still keeps
        // the door shut, because the identity is in the record it reads.
        let e = w
            .node_attach("pvc-d", "nqn:node-doomed", "node-doomed", 3)
            .await
            .unwrap_err();
        assert!(matches!(e.refusal(), Some(ExtentAllocError::FencedClient)), "{e}");
        assert_eq!(w.fenced_all().await.unwrap(), vec![("pvc-d".to_string(), 9u64)]);

        // And unfencing reopens it.
        assert!(w.unfence("pvc-d", 9).await.unwrap());
        w.node_attach("pvc-d", "nqn:node-doomed", "node-doomed", 4).await.unwrap();
    }

    /// A no-op writes NOTHING. Every reconcile pass on every target
    /// re-asserts the same facts; if each of those were a write, a
    /// fleet's steady state would be permanent API traffic and
    /// permanent contention on the one object failover depends on.
    #[tokio::test]
    async fn re_asserting_an_unchanged_fact_does_not_write() {
        let (w, store) = witness();
        w.target_register("node-a", "10.0.0.1", 4420, 1).await.unwrap();
        w.seat_volume("pvc-n", "node-a", 1, 100).await.unwrap();
        let writes = *store.puts.lock().unwrap();
        for _ in 0..5 {
            w.target_register("node-a", "10.0.0.1", 4420, 9).await.unwrap();
            w.seat_volume("pvc-n", "node-a", 9, 900).await.unwrap();
            w.leg_mark("pvc-n", "node-a", LEG_INSYNC, 1).await.unwrap();
        }
        assert_eq!(*store.puts.lock().unwrap(), writes, "steady state is READS only");
    }

    /// An unreadable document is not an empty one. Treating it as empty
    /// would re-seat a live volume and mint a second composition — the
    /// worst thing this file could do.
    #[tokio::test]
    async fn a_corrupt_record_refuses_rather_than_reading_as_absent() {
        let (w, store) = witness();
        w.seat_volume("pvc-c", "node-a", 1, 100).await.unwrap();
        let name = object_name(KIND_COMPOSITION, "pvc-c");
        store.docs.lock().unwrap().insert(name, ("{not json".into(), 7));
        let e = w.volume_seat("pvc-c").await.unwrap_err();
        assert!(matches!(e.refusal(), Some(ExtentAllocError::Corruption(_))), "{e}");
        assert!(!e.is_unreachable(), "a corrupt record is an ANSWER, not a cut");
    }

    /// Object names are DNS-1123 and collision-free: two ids that
    /// sanitize to the same string still get different objects.
    #[test]
    fn object_names_are_safe_and_do_not_collide() {
        let a = object_name(KIND_COMPOSITION, "pvc-Weird_Name/1");
        let b = object_name(KIND_COMPOSITION, "pvc-Weird-Name-1");
        assert_ne!(a, b, "the hash keeps sanitized twins apart");
        for n in [&a, &b] {
            assert!(n.len() <= 253, "{n}");
            assert!(
                n.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.'),
                "{n} is not DNS-1123"
            );
            assert!(!n.starts_with('-') && !n.ends_with('-'), "{n}");
        }
    }

    /// A TARGET whose id carries the composition infix is still a
    /// target — and the bug this names was invisible to every test in
    /// this module, because the fake store classified by PREFIX while
    /// the real one sniffed `-c-` anywhere in the name.
    ///
    /// The damage was not cosmetic. A mislabelled target is absent from
    /// `target_list`, so nothing can dial it, the prober has no subject
    /// for it, and a promotion to it is refused as an unregistered
    /// candidate: a healthy replica that silently cannot be failed over
    /// to, on nothing worse than a node named `gke-c-1-pool`.
    #[tokio::test]
    async fn a_target_named_like_a_composition_is_still_a_target() {
        let (w, _store) = witness();
        for id in ["mds-c-2", "gke-c-1-pool", "plain-target"] {
            assert_eq!(
                kind_of_name(&object_name(KIND_TARGET, id)),
                KIND_TARGET,
                "{id} classified as a composition"
            );
            w.target_register(id, "10.0.0.9", 4420, 100).await.unwrap();
        }
        let listed: Vec<String> =
            w.target_list().await.unwrap().into_iter().map(|t| t.target_id).collect();
        assert_eq!(
            listed,
            vec!["gke-c-1-pool", "mds-c-2", "plain-target"],
            "a target vanished from the registry on the strength of its NAME"
        );
    }
}
