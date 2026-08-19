//! Client-activity tracking — the hub's own answer to "is anyone using
//! this volume?".
//!
//! The lifecycle controller suspends an idle hub (pod scaled to zero,
//! PVC kept) and later hibernates it. It needs a signal that is true to
//! human/agent intent, and the honest one can only come from the server:
//! a mounted NFSv4.1 client keeps its lease alive with bare SEQUENCE
//! compounds forever, so "the session exists" says nothing about use.
//!
//! ## What counts, and why the exclusions are the whole design
//!
//! A compound is activity if it carries at least one op from the counted
//! set below. Deliberately EXCLUDED:
//!
//! - **bare SEQUENCE** — lease renewal, emitted by an idle mount on a
//!   timer. Counting it means no mounted hub ever idles.
//! - **GETATTR / ACCESS / VERIFY / NVERIFY** — kubelet's volume-stats
//!   collector statfs's every mounted volume about once a minute, and
//!   kernel attribute-cache revalidation fires these with no human
//!   involved. Same failure: a hub that never sleeps.
//! - **state maintenance** (CLOSE, EXCHANGE_ID, CREATE_SESSION,
//!   DESTROY_*, RECLAIM_COMPLETE, TEST/FREE_STATEID, SECINFO*) — mount,
//!   unmount and reconnect churn must not read as usage.
//! - **filehandle plumbing** (PUTFH/PUTROOTFH/GETFH/SAVEFH/RESTOREFH) —
//!   it always rides along with a counted op when real work happens.
//!
//! The bias is deliberate: READDIR and LOOKUP count, so a file manager
//! refreshing or a `find` sweep keeps the hub awake. Erring toward
//! not-suspending-under-a-user is the cheap mistake; suspending under
//! one strands a mount.
//!
//! The counters are relaxed atomics in the meter idiom
//! ([`crate::pnfs::mds::f68a_meter`]) and the classification is one scan
//! per COMPOUND — not per operation — so the hot path pays a branch.

use crate::nfs::v4::compound::Operation;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// What kind of use a compound represents. Ordered by strength: a
/// compound carrying ops from several classes reports the strongest.
#[derive(Clone, Copy, PartialEq, Eq, Debug, PartialOrd, Ord)]
pub enum ActivityClass {
    /// Directory listing / name resolution — browsing.
    Browse,
    /// Namespace mutation and open/lock state.
    Namespace,
    /// Content movement, or the pNFS layout ops that stand in for it.
    Data,
}

static LAST_ACTIVITY_UNIX: AtomicU64 = AtomicU64::new(0);
static DATA_OPS: AtomicU64 = AtomicU64::new(0);
static NAMESPACE_OPS: AtomicU64 = AtomicU64::new(0);
static BROWSE_OPS: AtomicU64 = AtomicU64::new(0);

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Seed the clock at startup, so a freshly woken hub reports "idle for
/// 0s" and gets a full idle window before it can be suspended again.
pub fn init() {
    LAST_ACTIVITY_UNIX.store(now_unix(), Relaxed);
}

/// Classify one COMPOUND's operation list. `None` = not activity.
pub fn classify(ops: &[Operation]) -> Option<ActivityClass> {
    let mut best: Option<ActivityClass> = None;
    for op in ops {
        let class = match op {
            // Content movement.
            Operation::Read { .. }
            | Operation::ReadPlus { .. }
            | Operation::Write { .. }
            | Operation::Commit { .. }
            | Operation::Allocate { .. }
            | Operation::Deallocate { .. }
            | Operation::Seek { .. }
            | Operation::Copy { .. }
            | Operation::Clone { .. } => ActivityClass::Data,

            // pNFS: the data path bypasses this server entirely, so
            // LAYOUTGET/LAYOUTCOMMIT are the ONLY evidence an MDS ever
            // sees of a client reading or writing gigabytes. Excluding
            // them would make a busy pNFS MDS look perfectly idle.
            Operation::LayoutGet { .. } | Operation::LayoutCommit { .. } => ActivityClass::Data,

            // Namespace and open/lock state.
            Operation::Open { .. }
            | Operation::OpenDowngrade { .. }
            | Operation::Create { .. }
            | Operation::Remove { .. }
            | Operation::Rename { .. }
            | Operation::Link { .. }
            | Operation::SetAttr { .. }
            | Operation::Lock { .. }
            | Operation::LockU { .. } => ActivityClass::Namespace,

            // Looking around.
            Operation::ReadDir { .. }
            | Operation::Lookup { .. }
            | Operation::LookupP
            | Operation::ReadLink => ActivityClass::Browse,

            // Everything else is renewal, cache revalidation, state
            // maintenance or filehandle plumbing — see the module doc.
            _ => continue,
        };
        best = Some(best.map_or(class, |b| b.max(class)));
        if best == Some(ActivityClass::Data) {
            break;
        }
    }
    best
}

/// Record one unit of client activity. Also the entry point for the
/// hub's file API, whose calls are real user intent even though they
/// never touch the wire.
pub fn note(class: ActivityClass) {
    LAST_ACTIVITY_UNIX.store(now_unix(), Relaxed);
    match class {
        ActivityClass::Data => &DATA_OPS,
        ActivityClass::Namespace => &NAMESPACE_OPS,
        ActivityClass::Browse => &BROWSE_OPS,
    }
    .fetch_add(1, Relaxed);
}

/// Classify and record in one call — the COMPOUND dispatch site.
pub fn note_compound(ops: &[Operation]) {
    if let Some(class) = classify(ops) {
        note(class);
    }
}

/// Unix seconds of the last counted activity (0 before [`init`]).
pub fn last_activity_unix() -> u64 {
    LAST_ACTIVITY_UNIX.load(Relaxed)
}

/// Seconds since the last counted activity.
///
/// Reports 0 when the clock was never seeded or has run backwards —
/// never a huge number. The controller suspends on this value, so an
/// uninitialised reading must not look like "idle since 1970".
pub fn idle_secs() -> u64 {
    let last = last_activity_unix();
    if last == 0 {
        return 0;
    }
    now_unix().saturating_sub(last)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivitySnapshot {
    pub last_activity_unix: u64,
    pub idle_secs: u64,
    pub data_ops: u64,
    pub namespace_ops: u64,
    pub browse_ops: u64,
}

pub fn snapshot() -> ActivitySnapshot {
    ActivitySnapshot {
        last_activity_unix: last_activity_unix(),
        idle_secs: idle_secs(),
        data_ops: DATA_OPS.load(Relaxed),
        namespace_ops: NAMESPACE_OPS.load(Relaxed),
        browse_ops: BROWSE_OPS.load(Relaxed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nfs::v4::compound::Operation;
    use crate::nfs::v4::protocol::{SessionId, StateId};

    fn seq() -> Operation {
        Operation::Sequence {
            sessionid: SessionId([0u8; 16]),
            sequenceid: 1,
            slotid: 0,
            highest_slotid: 0,
            cachethis: false,
        }
    }

    /// The load-bearing exclusion: an idle mount renews its lease
    /// forever, and the kubelet stats collector GETATTRs the root about
    /// once a minute. If either counted, no mounted hub would ever be
    /// suspendable — the whole lifecycle ladder would be dead.
    #[test]
    fn renewal_and_attribute_polling_are_not_activity() {
        assert_eq!(classify(&[seq()]), None);
        assert_eq!(classify(&[seq(), Operation::GetAttr(vec![])]), None);
        assert_eq!(classify(&[seq(), Operation::PutRootFh, Operation::GetAttr(vec![])]), None);
        assert_eq!(classify(&[seq(), Operation::Access(0x3f)]), None);
    }

    #[test]
    fn real_work_is_classified_by_strength() {
        let readdir = Operation::ReadDir {
            cookie: 0,
            cookieverf: [0u8; 8],
            dircount: 0,
            maxcount: 4096,
            attr_request: vec![],
        };
        assert_eq!(classify(&[seq(), Operation::Lookup("x".into())]), Some(ActivityClass::Browse));
        assert_eq!(classify(&[seq(), readdir]), Some(ActivityClass::Browse));
        assert_eq!(classify(&[seq(), Operation::Remove("x".into())]), Some(ActivityClass::Namespace));
        // A browse op alongside a data op reports the stronger class.
        assert_eq!(
            classify(&[
                seq(),
                Operation::Lookup("x".into()),
                Operation::Read { stateid: StateId::ANONYMOUS, offset: 0, count: 1 },
            ]),
            Some(ActivityClass::Data)
        );
    }

    #[test]
    fn note_advances_the_clock_and_the_class_counter() {
        init();
        let before = snapshot();
        note(ActivityClass::Browse);
        let after = snapshot();
        assert_eq!(after.browse_ops, before.browse_ops + 1);
        assert!(after.last_activity_unix >= before.last_activity_unix);
        assert_eq!(after.idle_secs, 0);
    }
}
