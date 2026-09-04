//! `/status`, in the shape the lite operator's ladder already parses
//! (`lite_operator::hubstatus`).
//!
//! The ladder suspends a workload only on a document it has read: a
//! poll that fails Holds forever, and there is no default to fall back
//! on. Forge therefore serves the same document rather than inventing
//! a second one, and the operator's suspend predicate — quiescible
//! phase AND idle past the threshold AND `rpoClean` — needs no new
//! code to reach forge.
//!
//! `rpoClean` is `true` whenever this syncer is serving with its lease
//! and its snapshot, because forge's acknowledgement rule makes it
//! true by construction: no push is acknowledged that the bucket does
//! not already hold (design §4). It is `false` while starting or
//! restoring, and never `null` — forge always has a tier, so "this
//! question does not apply" would be a lie the ladder acts on.

use super::Syncer;

/// The lifecycle phases forge reports. The names are
/// `hubstatus::HubPhase`'s, so an operator that does not know forge
/// still parses them; a phase outside the set is what its `Unknown`
/// arm is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Starting,
    ClaimingEpoch,
    /// Fetching packs and installing refs from the snapshot.
    Importing,
    Serving,
    Sweeping,
    Draining,
    Released,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Starting => "starting",
            Phase::ClaimingEpoch => "claimingEpoch",
            Phase::Importing => "importing",
            Phase::Serving => "serving",
            Phase::Sweeping => "sweeping",
            Phase::Draining => "draining",
            Phase::Released => "released",
        }
    }
}

/// A copy of everything `/status` reports, taken at a moment the
/// serving loop chooses.
///
/// The document is NOT cached, only these facts are: `idleSecs` is the
/// half of the ladder's AND that must be computed against the reader's
/// clock. Caching the rendered document would freeze it, and a frozen
/// `idleSecs` reads as "busy forever" or, worse, "idle forever".
#[derive(Debug, Clone)]
pub struct Facts {
    pub phase: Phase,
    pub holder_id: String,
    pub started_unix: u64,
    pub last_push_unix: u64,
    pub lease_epoch: Option<u64>,
    pub cell_loaded: bool,
    pub refs: usize,
    pub packs: usize,
    pub snapshot_seq: u64,
    pub fenced: Option<String>,
}

pub fn facts(sc: &Syncer, phase: Phase) -> Facts {
    Facts {
        phase,
        holder_id: sc.holder_id.clone(),
        started_unix: sc.started_unix,
        last_push_unix: sc.last_push_unix,
        lease_epoch: sc.lease.as_ref().map(|l| l.epoch),
        cell_loaded: sc.cell.is_some(),
        refs: sc.cell.as_ref().map(|c| c.snap.refs.len()).unwrap_or(0),
        packs: sc.cell.as_ref().map(|c| c.snap.packs.len()).unwrap_or(0),
        snapshot_seq: sc.cell.as_ref().map(|c| c.snap.seq).unwrap_or(0),
        fenced: sc.fenced.clone(),
    }
}

pub fn document(f: &Facts, now: u64) -> serde_json::Value {
    let last = if f.last_push_unix > 0 { f.last_push_unix } else { f.started_unix };
    let held = f.lease_epoch.is_some() && f.fenced.is_none();
    serde_json::json!({
        "phase": f.phase.as_str(),
        "uptimeSecs": now.saturating_sub(f.started_unix),
        "serverId": f.holder_id,
        "activity": {
            "lastActivityUnix": last,
            "idleSecs": now.saturating_sub(last),
        },
        // True by construction while serving: acknowledged means
        // durable. False until the repository is claimed and proved.
        "rpoClean": held && f.cell_loaded && f.phase == Phase::Serving,
        "epoch": { "held": held, "number": f.lease_epoch.unwrap_or(0) },
        "repo": { "refs": f.refs, "packs": f.packs, "snapshotSeq": f.snapshot_seq },
        "syncerVersion": super::SYNCER_VERSION,
        // Set ⇒ this server has been deposed and serves nothing. The
        // operator reads it as it reads `importRefused`: a stated
        // refusal, never an absence.
        "fenced": f.fenced,
    })
}
