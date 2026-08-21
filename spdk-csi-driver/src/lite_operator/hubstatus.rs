//! What the operator learns by asking the hub itself.
//!
//! Kubernetes knows whether a pod is Ready. It does not know whether the
//! volume's bytes are safely in the bucket, whether anyone is using the
//! share, or whether a DR import is halfway through — and every one of
//! those is a question the lifecycle ladder has to answer before it
//! scales a hub to zero or deletes a PVC. So the operator polls the
//! hub's own `/status`.
//!
//! ## Why the POD IP and not the Service
//!
//! The share's Service carries NFS and may be a LoadBalancer. Putting
//! the status port on it would publish the hub's file API — a
//! read-write surface over the whole volume — to whatever that
//! LoadBalancer faces. The operator already lists this share's pods for
//! the adoption fence, so it dials the pod directly on the cluster
//! network. Nothing outside the cluster can reach it.
//!
//! ## Why a failed poll is never "idle"
//!
//! The single most dangerous mistake this module could make is to
//! return a default on error. A hub that cannot be reached is not an
//! idle hub — it is an unknown hub, and the suspend predicate must
//! decline to act on it. So this returns `Result`, has no `Default`,
//! and the caller has nothing to fall back on.

use serde::Deserialize;
use std::time::Duration;

/// The hub's lifecycle phase, as `/status` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HubPhase {
    Starting,
    ClaimingEpoch,
    Importing,
    Reconciling,
    Serving,
    Sweeping,
    Draining,
    Released,
    /// A phase this operator build does not know. Forward compatibility:
    /// a newer hub must not make an older operator crash-loop, and an
    /// unknown phase is treated as "not safe to act on".
    #[serde(other)]
    Unknown,
}

impl HubPhase {
    /// Is the hub in a state where suspending it is even a question?
    ///
    /// Only `Serving`. `Sweeping` serves clients, but a sweep is
    /// unfinished work that would be interrupted — it resumes, so this
    /// is a politeness rather than a correctness rule, but suspending a
    /// hub mid-sweep to save a few cents of compute and then paying a
    /// full prefix LIST again on wake is a bad trade.
    pub fn is_quiescible(self) -> bool {
        self == HubPhase::Serving
    }
}

/// The subset of `/status` the operator acts on.
///
/// Deliberately not the whole document: `#[serde(default)]` on the
/// optional halves means a hub older or newer than this operator still
/// parses, and the fields that matter are the ones with no default.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HubSnapshot {
    pub phase: HubPhase,
    /// The persisted NFS server identity. STABLE across restarts on the
    /// same state, and DIFFERENT after a hibernate wakes onto a fresh
    /// PVC — which is the event that invalidates every client stateid.
    /// `None` = a hub too old to report it.
    #[serde(default)]
    pub server_id: Option<String>,
    #[serde(default)]
    pub uptime_secs: u64,
    #[serde(default)]
    pub activity: Activity,
    /// `None` = the hub has no tier, so there IS no recovery point.
    /// Never conflate with `Some(false)`: one means "this question does
    /// not apply", the other means "the answer is no".
    #[serde(default)]
    pub rpo_clean: Option<bool>,
    #[serde(default)]
    pub rpo: Option<Rpo>,
    #[serde(default)]
    pub epoch: Option<Epoch>,
    #[serde(default)]
    pub sweep: Option<Sweep>,
    /// The NFSv4 layer's own view. `None` = a hub too old to report it,
    /// which must never read as "no clients".
    #[serde(default)]
    pub nfs: Option<Nfs>,
    /// Set ⇒ the bucket holds a manifest the hub could not read, so the
    /// namespace was NOT restored. Nothing may publish from this hub.
    #[serde(default)]
    pub import_refused: Option<String>,
    /// The tier sub-document. NESTED, because that is where the hub
    /// actually puts it (`status.rs` `TierDoc`) — reading `gauges` off
    /// the top level parsed cleanly and silently yielded None forever,
    /// which is how auto-expand shipped inert and how the
    /// HydrationUnblocked condition reported "fine" without ever having
    /// looked. serde's `#[serde(default)]` makes a wrong path
    /// indistinguishable from an absent value, so this shape is now
    /// pinned by a test against a REAL document.
    #[serde(default)]
    pub tier: Option<TierDoc>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TierDoc {
    #[serde(default)]
    pub gauges: Option<Gauges>,
}

impl HubSnapshot {
    /// The tier gauges, wherever they live in the document.
    pub fn gauges(&self) -> Option<&Gauges> {
        self.tier.as_ref()?.gauges.as_ref()
    }
}

/// The subset of the hub's tier gauges the operator acts on. Everything
/// else in that document stays the hub's business; adding a field here
/// is a decision to make the operator depend on it.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Gauges {
    /// Objects larger than this PVC can ever hold. Every touch of one
    /// answers NOSPC and no eviction will ever change that, so it is a
    /// sizing fault the operator should say out loud rather than a
    /// transient the hub will work through.
    #[serde(default)]
    pub hydration_blocked: usize,
    /// What the manifest says the project costs to hold, and the single
    /// largest object in it. `None` on a hub that has not read or built
    /// a manifest yet — never conflate with zero.
    #[serde(default)]
    pub logical_bytes: Option<u64>,
    #[serde(default)]
    pub largest_object_bytes: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Activity {
    pub idle_secs: u64,
    #[serde(default)]
    pub data_ops: u64,
    #[serde(default)]
    pub namespace_ops: u64,
    #[serde(default)]
    pub browse_ops: u64,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rpo {
    #[serde(default)]
    pub dirty_files: usize,
    #[serde(default)]
    pub tombstones: usize,
    #[serde(default)]
    pub manifest_current: bool,
    #[serde(default)]
    pub beyond_rpo: Option<usize>,
}

/// What the NFSv4 state manager knows about live clients.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Nfs {
    /// Unexpired NFSv4 leases. `None` = the hub did not say, which is
    /// NOT zero: an absent count must never be read as "nobody is
    /// mounted" by a predicate that scales hubs to zero.
    #[serde(default)]
    pub active_leases: Option<usize>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Epoch {
    #[serde(default)]
    pub held: bool,
    #[serde(default)]
    pub number: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Sweep {
    #[serde(default)]
    pub completed: bool,
    #[serde(default)]
    pub stubs_created: usize,
    #[serde(default)]
    pub failed: usize,
}

impl HubSnapshot {
    /// May this share be scaled to zero right now, as far as the HUB is
    /// concerned?
    ///
    /// This is only ONE of the two signals the suspend decision needs —
    /// see the reconciler, which also requires the front door's
    /// heartbeat to be stale. Each covers the other's blind spot: an
    /// agent computing in memory for twenty minutes looks idle from
    /// here, and a workload that mounted without the front door in the
    /// loop has no heartbeat at all.
    pub fn suspendable(&self, idle_after_secs: u64) -> Result<(), String> {
        if !self.phase.is_quiescible() {
            return Err(format!("hub phase is {:?}, not Serving", self.phase));
        }
        if let Some(why) = &self.import_refused {
            return Err(format!("import was refused ({why}) — the export is not the bucket's tree"));
        }
        if self.activity.idle_secs < idle_after_secs {
            return Err(format!(
                "last client activity {}s ago, under the {}s threshold",
                self.activity.idle_secs, idle_after_secs
            ));
        }
        // A sweep that has not completed is unfinished work. It resumes
        // on wake, so this is not a correctness gate — but suspending
        // mid-sweep means paying the whole prefix LIST again.
        if self.sweep.is_some_and(|s| !s.completed) {
            return Err("a foreign-key sweep is still running".to_string());
        }
        Ok(())
    }

    /// Does a client still hold a lease on this hub?
    ///
    /// `None` = the hub did not report, so the answer is unknown and
    /// the caller must not infer "nobody". Feeds `Inputs::sessions_live`,
    /// which `spec.idle.suspendWithSessions: false` acts on.
    pub fn sessions_live(&self) -> Option<bool> {
        self.nfs
            .and_then(|n| n.active_leases)
            .map(|n| n > 0)
    }

    /// May the PVC be deleted — i.e. can the bucket rebuild this volume?
    ///
    /// `rpoClean: null` (no tier) is a REFUSAL, not a pass. An untiered
    /// share's PVC is the only copy of its data, and reading absence as
    /// "clean" would delete a project.
    pub fn hibernatable(&self) -> Result<(), String> {
        match self.rpo_clean {
            // `rpoClean` describes A volume. It is an answer about THIS
            // pod's volume only if this pod is the one holding the tier
            // epoch — and self-recognition is gated on the state
            // directory's occupancy lock, so a second live process on
            // the same PVC genuinely does not hold it. That makes
            // `held` a real discriminator rather than a restatement of
            // "the hub answered".
            //
            // The consequence of believing the wrong pod here is
            // `claims.delete`, so anything short of "this hub holds the
            // epoch" defers. Deferring is free: the next pass asks
            // again. (`rpo` is only computed when the guard exists, so
            // the `None` arm is unreachable from a well-formed status
            // doc — it is a refusal rather than an `unwrap` because the
            // hub is a separate process across a version boundary.)
            Some(true) => match self.epoch.as_ref() {
                Some(e) if e.held => Ok(()),
                Some(_) => Err(
                    "the hub does not hold the tier epoch — another process may own this volume"
                        .to_string(),
                ),
                None => Err(
                    "the hub reported a clean RPO but no tier epoch — refusing to reclaim a disk                      on an unattributed flush"
                        .to_string(),
                ),
            },
            Some(false) => Err(self
                .rpo
                .as_ref()
                .map(describe_rpo)
                .unwrap_or_else(|| "the bucket cannot rebuild this volume".to_string())),
            None => Err(
                "this share has no bucket — its PVC is the only copy of the data".to_string(),
            ),
        }
    }
}

fn describe_rpo(r: &Rpo) -> String {
    let mut why = Vec::new();
    if r.dirty_files > 0 {
        why.push(format!("{} unpublished file(s)", r.dirty_files));
    }
    if r.tombstones > 0 {
        why.push(format!("{} unapplied delete(s)", r.tombstones));
    }
    if !r.manifest_current {
        why.push("the bucket's manifest is behind the tree".to_string());
    }
    if let Some(n) = r.beyond_rpo.filter(|n| *n > 0) {
        why.push(format!("{n} file(s) beyond RPO"));
    }
    if why.is_empty() {
        "the bucket cannot rebuild this volume".to_string()
    } else {
        why.join("; ")
    }
}

/// Poll one hub.
///
/// Short timeout on purpose: this runs inside a reconcile, and a hub
/// that is slow to answer must not stall the whole controller's work
/// queue. A timeout is an error, and an error is "unknown", never
/// "idle".
pub async fn poll(pod_ip: &str, port: i32, timeout: Duration) -> Result<HubSnapshot, String> {
    let url = if pod_ip.contains(':') {
        // IPv6 literals need brackets.
        format!("http://[{pod_ip}]:{port}/status")
    } else {
        format!("http://{pod_ip}:{port}/status")
    };
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| format!("http client: {e}"))?;
    let res = client.get(&url).send().await.map_err(|e| format!("GET {url}: {e}"))?;
    if !res.status().is_success() {
        return Err(format!("GET {url}: HTTP {}", res.status()));
    }
    let body = res.text().await.map_err(|e| format!("body of {url}: {e}"))?;
    serde_json::from_str(&body).map_err(|e| format!("parsing {url}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(json: &str) -> HubSnapshot {
        serde_json::from_str(json).expect("the hub's own document must parse")
    }

    /// THE FLEET RIG'S ANTI-VACUITY GUARD.
    ///
    /// `flint-hub-stub` stands in for a real hub so 300 "live" shares
    /// fit on a small cluster. The failure mode that would quietly
    /// ruin every measurement taken with it: the stub's document
    /// drifts, `poll_hub` falls onto its `Err` branch, every share
    /// reads as unreachable — and the rig reports a beautifully stable
    /// fleet while measuring nothing at all.
    ///
    /// So the stub builds the REAL `StatusDoc`, and this pins that the
    /// operator can still read one, field by field for the fields the
    /// ladder actually acts on. A rename on either side fails here
    /// rather than on a cluster.
    #[test]
    fn the_fleet_rig_stubs_document_is_readable_by_the_operator() {
        use crate::nfs::activity::ActivitySnapshot;
        use crate::pnfs::mds::status::{EpochDoc, NfsDoc, StatusDoc, TierDoc};
        use crate::tier::rpo::RpoStatus;

        // Constructed exactly as src/bin/flint_hub_stub.rs does.
        let doc = StatusDoc {
            phase: crate::pnfs::mds::status::HubPhase::Serving,
            server_id: Some("stub-1".into()),
            pod_name: Some("tenant-a-abc".into()),
            started_unix: 1_000,
            uptime_secs: 42,
            epoch: Some(EpochDoc { held: true, number: Some(1) }),
            import: None,
            sweep: None,
            import_refused: None,
            warm_fill: None,
            tier: TierDoc { gauges: None, meters: Default::default() },
            nfs: NfsDoc { active_leases: Some(3) },
            activity: ActivitySnapshot {
                last_activity_unix: 900,
                idle_secs: 1234,
                data_ops: 0,
                namespace_ops: 0,
                browse_ops: 0,
            },
            rpo_clean: Some(true),
            rpo: Some(RpoStatus {
                clean: true,
                dirty_files: 0,
                pending_capture: false,
                tombstones: 0,
                epoch_held: true,
                manifest_current: true,
                manifest_seq: Some(1),
                beyond_rpo: Some(0),
                awaiting_first_barrier: false,
            }),
        };

        let wire = serde_json::to_string(&doc).expect("the stub must serialize");
        let got: HubSnapshot = serde_json::from_str(&wire)
            .expect("the OPERATOR must be able to read the stub's document");

        // The fields the ladder acts on — not a smoke test.
        assert_eq!(got.phase, HubPhase::Serving, "phase drives the suspend gate");
        assert_eq!(got.activity.idle_secs, 1234, "idleSecs is the hub's half of the AND");
        assert_eq!(got.rpo_clean, Some(true), "rpoClean authorizes deleting a PVC");
        assert_eq!(got.server_id.as_deref(), Some("stub-1"));
        assert!(got.hibernatable().is_ok(), "a clean stub must be hibernatable");
        assert!(
            got.suspendable(600).is_ok(),
            "a stub idle for 1234s must be suspendable against a 600s threshold"
        );
    }

    /// The document the hub actually emits — pinned here so a field
    /// rename on either side fails the suite instead of silently
    /// defaulting to a value the ladder then acts on.
    #[test]
    fn the_hubs_own_status_document_parses() {
        let doc = serde_json::to_string(&serde_json::json!({
            "phase": "serving",
            "startedUnix": 1_700_000_000u64,
            "uptimeSecs": 3600,
            "epoch": { "held": true, "number": 7 },
            "import": null,
            "warmFill": null,
            "sweep": { "scanned": 12, "stubsCreated": 12, "skippedTombstoned": 0,
                       "skippedKnown": 0, "skippedLocalExists": 0, "failed": 0,
                       "completed": true },
            "tier": { "gauges": null, "meters": {} },
            "nfs": { "activeLeases": 2 },
            "activity": { "lastActivityUnix": 1_700_003_000u64, "idleSecs": 600,
                          "dataOps": 12, "namespaceOps": 3, "browseOps": 41 },
            "rpoClean": true,
            "rpo": { "clean": true, "dirtyFiles": 0, "pendingCapture": false,
                     "tombstones": 0, "epochHeld": true, "manifestCurrent": true,
                     "manifestSeq": 9, "beyondRpo": 0, "awaitingFirstBarrier": false }
        }))
        .unwrap();
        let s = snap(&doc);
        assert_eq!(s.phase, HubPhase::Serving);
        assert_eq!(s.activity.idle_secs, 600);
        assert_eq!(s.rpo_clean, Some(true));
        assert!(s.epoch.as_ref().unwrap().held);
        assert!(s.sweep.unwrap().completed);
        assert!(s.hibernatable().is_ok());
        assert!(s.suspendable(300).is_ok());
    }

    /// A hub too new for this operator must not make it crash-loop, and
    /// an unrecognised phase must not read as safe.
    #[test]
    fn an_unknown_phase_is_not_actionable() {
        let s = snap(r#"{"phase":"somethingNewInV2","activity":{"idleSecs":99999}}"#);
        assert_eq!(s.phase, HubPhase::Unknown);
        assert!(!s.phase.is_quiescible());
        assert!(s.suspendable(60).is_err());
    }

    /// **No tier ⇒ the PVC is the only copy.** `rpoClean: null` must
    /// never read as permission to delete it. This is the assertion
    /// `activeLeases` has to survive the trip from the hub's document
    /// into the predicate, and an ABSENT count must not read as zero —
    /// the caller scales hubs to zero on this.
    #[test]
    fn a_missing_lease_count_is_unknown_rather_than_nobody() {
        let mounted = snap(r#"{"phase":"serving","nfs":{"activeLeases":2}}"#);
        assert_eq!(mounted.sessions_live(), Some(true));

        let empty = snap(r#"{"phase":"serving","nfs":{"activeLeases":0}}"#);
        assert_eq!(empty.sessions_live(), Some(false));

        // A hub too old to report it, and a hub reporting nfs with no
        // count, are both UNKNOWN.
        let silent = snap(r#"{"phase":"serving"}"#);
        assert_eq!(silent.sessions_live(), None, "absent must not read as nobody");
        let partial = snap(r#"{"phase":"serving","nfs":{}}"#);
        assert_eq!(partial.sessions_live(), None);
    }

    /// A clean RPO from a hub that does NOT hold the tier epoch is an
    /// answer about someone else's volume. The consequence of getting
    /// this wrong is `claims.delete`, so it defers instead.
    #[test]
    fn a_clean_rpo_without_the_epoch_is_not_hibernatable() {
        let held = snap(
            r#"{"phase":"serving","rpoClean":true,"epoch":{"held":true,"number":7},
                "activity":{"idleSecs":99999}}"#,
        );
        assert!(held.hibernatable().is_ok(), "the epoch holder's clean RPO must pass");

        let not_held = snap(
            r#"{"phase":"serving","rpoClean":true,"epoch":{"held":false,"number":7},
                "activity":{"idleSecs":99999}}"#,
        );
        let err = not_held
            .hibernatable()
            .expect_err("a non-holder must NOT be able to authorise a delete");
        assert!(err.contains("epoch"), "{err}");

        // A clean RPO with no epoch at all cannot come from a healthy
        // hub (rpo is only computed when the guard exists), so it is
        // unattributed rather than trustworthy.
        let no_epoch = snap(r#"{"phase":"serving","rpoClean":true,"activity":{"idleSecs":99999}}"#);
        assert!(
            no_epoch.hibernatable().is_err(),
            "an unattributed clean RPO must not authorise a delete"
        );

        // Suspend is unaffected: it keeps the PVC either way.
        assert!(not_held.suspendable(60).is_ok());
    }

    /// that stands between a bug here and a deleted project.
    #[test]
    fn a_tierless_share_is_never_hibernatable() {
        let s = snap(r#"{"phase":"serving","rpoClean":null,"activity":{"idleSecs":99999}}"#);
        let err = s.hibernatable().expect_err("a share with no bucket must NOT be hibernatable");
        assert!(err.contains("only copy"), "{err}");
        // But it IS suspendable — suspend keeps the PVC, so it is safe
        // for exactly the share hibernate is not.
        assert!(s.suspendable(60).is_ok());
    }

    #[test]
    fn a_dirty_hub_explains_itself_rather_than_just_refusing() {
        let s = snap(
            r#"{"phase":"serving","rpoClean":false,
                "rpo":{"dirtyFiles":3,"tombstones":1,"manifestCurrent":false,"beyondRpo":2},
                "activity":{"idleSecs":99999}}"#,
        );
        let err = s.hibernatable().unwrap_err();
        for want in ["3 unpublished", "1 unapplied", "manifest is behind", "2 file(s) beyond"] {
            assert!(err.contains(want), "{err} is missing {want}");
        }
    }

    /// Startup phases are minutes long and are PROGRESS, not idleness.
    /// A hub scaled to zero mid-import loses the import and pays for it
    /// again on wake.
    #[test]
    fn a_hub_still_starting_is_never_suspended() {
        for phase in ["starting", "claimingEpoch", "importing", "reconciling", "draining"] {
            let s = snap(&format!(
                r#"{{"phase":"{phase}","activity":{{"idleSecs":99999}}}}"#
            ));
            assert!(
                s.suspendable(60).is_err(),
                "phase {phase} must not be suspendable even when idle"
            );
        }
    }

    #[test]
    fn recent_activity_blocks_the_suspend() {
        let s = snap(r#"{"phase":"serving","activity":{"idleSecs":42}}"#);
        let err = s.suspendable(900).unwrap_err();
        assert!(err.contains("42s ago"), "{err}");
        assert!(s.suspendable(10).is_ok(), "past the threshold it is fine");
    }

    /// A refused import means the export is NOT the bucket's tree.
    /// Suspending is harmless in itself, but the ladder's next rung
    /// deletes the PVC, and letting a share on this path advance is how
    /// a recoverable bucket error becomes a lost project.
    #[test]
    fn a_refused_import_blocks_the_ladder() {
        let s = snap(
            r#"{"phase":"serving","importRefused":"unparseable: bad json",
                "activity":{"idleSecs":99999}}"#,
        );
        let err = s.suspendable(60).unwrap_err();
        assert!(err.contains("import was refused"), "{err}");
    }

    #[test]
    fn an_unfinished_sweep_defers_the_suspend() {
        let s = snap(
            r#"{"phase":"serving","sweep":{"completed":false},"activity":{"idleSecs":99999}}"#,
        );
        assert!(s.suspendable(60).is_err());
    }
}

#[cfg(test)]
mod shape_tests {
    use super::*;

    /// A REAL `/status`, captured from a hub running in the kind drill.
    ///
    /// This fixture exists because of a bug it would have caught for
    /// free. `HubSnapshot` declared `gauges` at the top level; the hub
    /// publishes them under `tier`. Every field is `#[serde(default)]`
    /// — as it must be, so an older hub's missing fields do not fail
    /// the parse — which means a WRONG PATH deserializes cleanly and
    /// yields `None` forever. Auto-expand shipped inert that way, and
    /// the HydrationUnblocked condition reported "fine" without ever
    /// having looked at anything.
    ///
    /// There WAS already a hand-written fixture here, and it had the
    /// nesting right — but it set `"gauges": null`, so no assertion
    /// ever pulled a value through that path and the wrong field
    /// declaration sat under it undisturbed. That is the lesson worth
    /// keeping: the fixture's shape was never the weak point, the
    /// missing assertion was. A test that parses a document without
    /// reading the field you depend on proves only that serde does not
    /// crash.
    ///
    /// Using a REAL document on top of that is belt-and-braces against
    /// the other half — a fixture invented from the same mistaken model
    /// as the code would agree with the bug. Refresh it by curling
    /// `/status` on a live hub.
    #[test]
    fn the_operator_reads_a_real_hub_status_document() {
        let raw = include_str!("../../tests/fixtures/hub-status.json");
        let snap: HubSnapshot =
            serde_json::from_str(raw).expect("a real /status must parse");

        assert_eq!(snap.phase, HubPhase::Serving);
        assert_eq!(snap.rpo_clean, Some(true));
        assert!(snap.server_id.is_some(), "serverId is part of the contract");

        // The whole point: the gauges must actually be FOUND.
        let g = snap
            .gauges()
            .expect("the tier gauges must be reachable — this is the bug");
        assert_eq!(
            g.logical_bytes,
            Some(629_145_600),
            "the project's size drives auto-expand and must not read as absent"
        );
        assert_eq!(g.largest_object_bytes, Some(104_857_600));
        assert_eq!(g.hydration_blocked, 0);
    }

    /// The other half of the trap: absence must stay distinguishable
    /// from zero. A hub too old to publish gauges yields None, and
    /// auto-expand must decline to size a disk against "I do not know"
    /// rather than treating it as an empty project.
    #[test]
    fn an_absent_tier_block_is_none_not_zero() {
        let snap: HubSnapshot =
            serde_json::from_str(r#"{"phase":"serving"}"#).expect("sparse doc parses");
        assert!(snap.gauges().is_none(), "no tier block ⇒ no gauges, not zeroed ones");
    }
}
