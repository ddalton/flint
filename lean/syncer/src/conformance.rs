//! Does this store actually enforce the conditions lean writes under?
//!
//! Everything the protocol promises rests on one property of the
//! backend: a conditional write is REFUSED when its condition does not
//! hold. The manifest CAS, the lease cell, the upload gate and the file
//! collector are all conditional writes, and a store that accepts the
//! header and ignores it answers byte-for-byte like one that enforces
//! it — until the day it overwrites or deletes the wrong generation.
//!
//! This was a documented assumption (`lean/SAFETY.md` §2) and a CLI verb
//! an operator could remember to run. That is not enough, and the model
//! says why in the sharpest possible terms: the garbage collector's
//! DELETE carries `If-Match` because the variant WITHOUT it —
//! `LeanBarrierLeaseGCUnconditional` — is refuted by the gate, a
//! mutation that must violate an invariant. On a store that ignores the
//! header, the shipped binary IS that mutation. Nothing about the code
//! is wrong; the store makes it wrong.
//!
//! So the probes run before the first barrier, and the answer is acted
//! on rather than printed:
//!
//! - **PUT conditionals broken** → refuse the workspace. `If-None-Match`
//!   and `If-Match` on PUT carry the manifest CAS and the lease itself;
//!   there is no degraded mode, only last-writer-wins pretending to be
//!   arbitration.
//! - **`If-Match` on DELETE broken** → refuse the COLLECTOR, not the
//!   workspace. The syncer runs, publishes and arbitrates exactly as
//!   before; the collector leaves its objects behind (`barrier.rs`,
//!   `report.leaked`). Uncited objects are served to nobody and swept by
//!   nothing, so the cost is storage growth. Apache Ozone 2.2.x is this
//!   case (HDDS-14907, finding L-27), and refusing it outright would
//!   refuse the on-prem target over a leak.
//!
//! The verdict is cached in the state dir per store identity: the probes
//! cost ~10 requests and a store does not change its mind between two
//! barriers. The state dir is the worker's emptyDir, so a store that is
//! FIXED is re-probed by the next pod without anyone clearing a cache.

use crate::state::StoreConformance;
use crate::{now_unix, Access, LeanResult, Syncer, LEAN_DIR};

/// Bump when a probe asks a NEW question, so an old verdict is never
/// read as the answer to it.
pub const PROBE_VERSION: u32 = 1;

/// What the gate decided, for the caller to act on and report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Both surfaces enforced (or a read-only workspace, which writes
    /// nothing and so stands on neither).
    Conformant,
    /// `If-Match` on DELETE is not enforced: the collector is off for
    /// this workspace and objects it would have taken are left behind.
    CollectorOff(String),
    /// A conditional PUT was ACCEPTED when it had to be refused. The
    /// workspace must not be published.
    Refuse(String),
    /// The probe could not write its own object, so the store answered
    /// nothing: a read-only credential, a bucket policy, an endpoint
    /// that is down. NOT a verdict — the verb runs exactly as it did
    /// before this gate existed and fails with its own, accurate error.
    ///
    /// Refusing here would be refusing an ERROR while reporting an
    /// ANSWER: `lean/e2e/access/read-grant-minio.sh` D1 runs a
    /// read-write syncer on read-only credentials on purpose, and the
    /// operator it hands a diagnosis to deserves "403", not "this store
    /// does not enforce conditional writes".
    Unknown(String),
}

/// Probe this store (or read the cached verdict), apply the result to
/// `sc.cfg`, and say what was decided.
///
/// `identity` names the store the verdict belongs to — endpoint, bucket
/// and prefix — so a workspace re-pointed somewhere else is asked again
/// rather than inheriting an answer about a different bucket.
pub async fn gate(sc: &mut Syncer, identity: &str) -> LeanResult<Decision> {
    // A read-only syncer never CASes a manifest, never claims the cell
    // and never collects. It also cannot run the probes, which write.
    if sc.cfg.access == Access::Read {
        return Ok(Decision::Conformant);
    }

    let cached = sc.state.load_conformance()?.filter(|c| {
        c.identity == identity && c.probe_version == PROBE_VERSION
    });
    let verdict = match cached {
        Some(c) => c,
        None => {
            let c = probe(sc, identity).await;
            // Only an ANSWER is cached. A probe that could not write at
            // all says nothing about the store, and a cached "no" would
            // outlive the outage that produced it.
            if c.answered {
                sc.state.save_conformance(&c)?;
            }
            c
        }
    };

    sc.trace(
        "conformance",
        serde_json::json!({
            "identity": verdict.identity, "answered": verdict.answered,
            "put": verdict.conditional_put, "delete": verdict.conditional_delete,
            "detail": verdict.detail,
        }),
    );

    // Could not write its own object: no verdict exists. Say so and get
    // out of the way — the verb will fail on the same credential with a
    // message that names it.
    if !verdict.answered {
        return Ok(Decision::Unknown(verdict.detail.unwrap_or_default()));
    }
    if !verdict.conditional_put {
        return Ok(Decision::Refuse(verdict.detail.unwrap_or_default()));
    }
    if !verdict.conditional_delete {
        sc.cfg.conditional_delete_enforced = false;
        return Ok(Decision::CollectorOff(verdict.detail.unwrap_or_default()));
    }
    Ok(Decision::Conformant)
}

/// The two probes, on keys nothing else uses.
///
/// The key carries a per-writer id because the probes are themselves
/// conditional writes: two conformant syncers sharing a workspace and a
/// probe key would fail each other's `If-None-Match` and each report the
/// other's conformance as its own broken store.
async fn probe(sc: &Syncer, identity: &str) -> StoreConformance {
    let who = std::env::var("HOSTNAME")
        .ok()
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let mut out = StoreConformance {
        identity: identity.to_string(),
        probe_version: PROBE_VERSION,
        answered: true,
        conditional_put: false,
        conditional_delete: false,
        detail: None,
        at_unix: now_unix(),
    };
    // A probe that could not write its own object did not ANSWER: that
    // is the credential or the network, and reporting it as a verdict
    // about conditional writes is how a 403 comes to read as "this store
    // is not conformant".
    let unanswered = |e: &str| e.starts_with(flint_store::probe::PROBE_UNREACHABLE);

    let put_key = format!("{}/{}/probe/conditional-put-{who}", sc.cfg.prefix, LEAN_DIR);
    if let Err(e) = flint_store::probe::probe_conditional_writes(sc.store.as_ref(), &put_key).await
    {
        out.answered = !unanswered(&e);
        out.detail = Some(e);
        return out;
    }
    out.conditional_put = true;

    let del_key = format!("{}/{}/probe/conditional-delete-{who}", sc.cfg.prefix, LEAN_DIR);
    match flint_store::probe::probe_conditional_delete(sc.store.as_ref(), &del_key).await {
        Ok(()) => out.conditional_delete = true,
        // The PUT probe just wrote here, so a write failure now is a
        // surprise rather than a credential. Still not a verdict about
        // the DELETE condition: leave the collector alone.
        Err(e) if unanswered(&e) => {
            out.answered = false;
            out.detail = Some(e);
        }
        Err(e) => out.detail = Some(e),
    }
    out
}

/// The operator-facing sentence for each outcome. Kept here, next to the
/// decision, so the CLI and the worker cannot describe the same verdict
/// two different ways.
pub fn message(d: &Decision, prefix: &str) -> String {
    match d {
        Decision::Conformant => "conditional writes enforced (PUT and DELETE)".into(),
        Decision::CollectorOff(why) => format!(
            "this store does not enforce If-Match on DELETE ({why}). The syncer runs and \
             publishes normally, but the file collector is OFF: an object a delete retires \
             is LEFT in the bucket rather than removed, because a delete this store applies \
             unconditionally could take the version another writer is about to cite (the \
             model's LeanBarrierLeaseGCUnconditional). Expect storage growth under \
             {prefix}/files/, and see lean/SAFETY.md §2"
        ),
        Decision::Refuse(why) => format!(
            "this store does not enforce conditional writes ({why}). Lean arbitrates \
             ENTIRELY through them — the manifest CAS, the publish fence and every upload \
             — so on this store two writers would silently overwrite each other instead of \
             conflicting. Refusing rather than degrading; see lean/SAFETY.md §2"
        ),
        Decision::Unknown(why) => format!(
            "could not ask this store whether it enforces conditional writes ({why}) — the \
             probe could not write its own object. Continuing UNVERIFIED, exactly as before \
             this check existed; the verb below will report the underlying error itself"
        ),
    }
}
