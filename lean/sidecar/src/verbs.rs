//! The one-shot verbs, and which of them takes the publish fence.
//!
//! Lifted out of `bin/flint_sync.rs` so the RULE — which verb claims —
//! is a property the battery can execute rather than a line of dispatch
//! nobody can reach from a test. `a_checkout_does_not_wait_out_a_\
//! standing_lease` in `tests.rs` calls `read_only_then`, the same
//! function the dispatch calls, rather than a re-implementation of it.

use std::time::Duration;

use super::lease::{self, ClaimOutcome};
use super::{LeanError, Sidecar};

/// A comma list, trimmed, empties dropped. Returns `None` when the
/// variable is unset or holds only separators — `Some(vec![])` would be
/// an empty scope, and `checkout_scoped` refuses that rather than let it
/// mean "everything".
pub fn env_list(name: &str) -> Option<Vec<String>> {
    let raw = std::env::var(name).ok()?;
    let v: Vec<String> =
        raw.split(',').map(|e| e.trim().to_string()).filter(|e| !e.is_empty()).collect();
    if v.is_empty() {
        return None;
    }
    Some(v)
}

pub enum Step {
    Checkout,
    Barrier,
    Sync,
    RecoverStaged,
    /// The narrow/widen verb. `None` is the whole tree; an empty
    /// argument list therefore cannot mean "narrow to nothing".
    Rescope(Option<Vec<String>>),
}

impl Step {
    /// Does this verb need the publish fence?
    ///
    /// The epoch cell arbitrates who may INSTALL a manifest. A verb
    /// that installs none does not belong in that queue, and putting it
    /// there does not make it safer — see `read_only_then` for what it
    /// actually cost.
    ///
    /// `checkout` is the only `false` today. `rescope` reads the bucket
    /// and writes nothing to it either, but it rewrites the held set
    /// and leaves a replayable intent behind, so it stays fenced until
    /// someone has walked that crash matrix with two of them running;
    /// `sync`, `barrier` and `recover-staged` all publish.
    pub fn installs_nothing_in_the_bucket(&self) -> bool {
        match self {
            Step::Checkout => true,
            Step::Barrier | Step::Sync | Step::RecoverStaged | Step::Rescope(_) => false,
        }
    }
}

/// The one door every one-shot verb goes through. The ROUTING lives
/// here rather than in the binary's argv match so that the battery
/// exercises the same decision the shipped binary makes.
pub async fn run_verb(sc: &mut Sidecar, step: Step) -> Result<(), LeanError> {
    if step.installs_nothing_in_the_bucket() {
        read_only_then(sc, step).await
    } else {
        claim_then(sc, step).await
    }
}

pub async fn claim(sc: &mut Sidecar) -> Result<(), LeanError> {
    // Before the first claim step: is this prefix ours to claim at all?
    lease::verify_claim(sc).await?;
    lease::warn_if_prefix_is_shared(sc).await;
    let mut answered_owed = false;
    loop {
        match lease::claim_step(sc).await? {
            ClaimOutcome::Claimed(lease) => {
                eprintln!("flint-sync: holding epoch {}", lease.epoch);
                return Ok(());
            }
            ClaimOutcome::Waiting { quiet_polls } => {
                if !answered_owed {
                    answered_owed = true;
                    match sc.refuse_what_this_incarnation_can_never_honor().await {
                        Ok(true) => eprintln!(
                            "flint-sync: a foreign holder stands and this incarnation owes an \
                             ack it can never honor — refused-fenced written, marker fenced"
                        ),
                        Ok(false) => {}
                        Err(e) => {
                            // Never let this block the claim: a fresh
                            // pod must still take over.
                            answered_owed = false;
                            eprintln!("flint-sync: could not settle owed acks while waiting: {e}");
                        }
                    }
                }
                eprintln!("flint-sync: waiting on the standing lease (quiet {quiet_polls}/6)");
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        }
    }
}

/// Take the writer's lease, do one step, release it.
///
/// Every verb that can change the BUCKET comes through here. `checkout`
/// deliberately does not — see `read_only_then`.
pub async fn claim_then(sc: &mut Sidecar, step: Step) -> Result<(), LeanError> {
    claim(sc).await?;
    let out = run_step(sc, step).await;
    let _ = lease::release(sc).await;
    out
}

/// A verb that reads the bucket and writes only this pod's own tree.
///
/// It does NOT claim the epoch, and that is the point. The epoch cell
/// is a PUBLISH fence: it decides which of several syncers may install
/// a manifest. A checkout installs nothing — it GETs the manifest and
/// GETs the objects it cites, and every one of those GETs already
/// carries the cited etag as `If-Match`, so a publisher racing a reader
/// yields a 412 (handled, loudly, in `checkout.rs`) and never a torn
/// file.
///
/// What claiming actually bought was a DEADLOCK dressed as mutual
/// exclusion. `claim_step` supersedes a foreign holder only after six
/// observations in which its token did not advance — and a LIVE holder
/// renews, so its token always advances. A checkout that met a running
/// publisher therefore waited forever, and a publisher that met a
/// checkout waited for it (forge's export already carries a timeout for
/// exactly this: `export.rs`, "waits for a foreign lease forever").
/// Measured 2026-09-11: four syncers reading disjoint subtrees of one
/// prefix ran 2.1x faster with the claim skipped, and 0.41x — SLOWER
/// than one syncer — with it, because they serialised on the cell.
///
/// Two things here are not the lease and stay:
///   * `verify_claim` — the project-id precondition. A refusal, not a
///     fence: this prefix is another project's and must not be read or
///     written by us at all.
///   * `warn_if_prefix_is_shared` — a diagnostic nobody else emits.
///
/// `status` and `ctl` have taken no lease since they shipped, for the
/// same reason stated the same way; this makes `checkout` the third.
pub async fn read_only_then(sc: &mut Sidecar, step: Step) -> Result<(), LeanError> {
    lease::verify_claim(sc).await?;
    lease::warn_if_prefix_is_shared(sc).await;
    run_step(sc, step).await
}

/// The verb bodies. Shared by both doors above so that the ONLY
/// difference between a fenced verb and a lease-free one is the door,
/// never a second copy of the work.
pub async fn run_step(sc: &mut Sidecar, step: Step) -> Result<(), LeanError> {
    {
        match step {
            Step::Checkout => {
                let r = sc.checkout_scoped(env_list("FLINT_SYNC_CHECKOUT_SCOPE")).await?;
                eprintln!(
                    "flint-sync: checkout — {} materialized, {} present, live-tree={}",
                    r.materialized, r.skipped_present, r.resumed_live_tree
                );
                if let Some(sc) = &r.scope {
                    // The declined count, not just the scope: a scope
                    // that admits everything reads exactly like no
                    // scope, and "it was configured" is not evidence
                    // that it did anything.
                    eprintln!(
                        "flint-sync: checkout SCOPED to {:?} — {} citations declined",
                        sc, r.out_of_scope
                    );
                }
                // BYTES on the same line as the phases, deliberately:
                // an A/B of the fetch window compares two wall clocks,
                // and an arm that "wins" by materialising fewer bytes
                // is the failure mode a timing-only line cannot show.
                eprintln!(
                    "flint-sync: phase manifest={:.3}s fetch={:.3}s commit={:.3}s bytes={} ranged={}",
                    r.manifest_secs, r.fetch_secs, r.commit_secs, r.bytes, r.ranged
                );
            }
            Step::Rescope(target) => {
                let r = sc.rescope(target).await?;
                eprintln!(
                    "flint-sync: rescope to {:?} — uncited {}, unlinked {}, materialised {} \
                     ({} bytes), already held {}",
                    r.target, r.uncited, r.unlinked, r.materialized, r.bytes, r.already_held
                );
                // A kept path is the one outcome a caller must not miss:
                // the scope now says one thing and the held set holds
                // more, and the reason is an edit only they can resolve.
                if !r.kept_dirty.is_empty() {
                    eprintln!(
                        "flint-sync: rescope KEPT {} path(s) with unpublished changes: {:?} — \
                         publish them, then rescope again to drop them",
                        r.kept_dirty.len(),
                        r.kept_dirty
                    );
                }
            }
            Step::Barrier => {
                let r = sc.run_barrier().await?;
                eprintln!(
                    "flint-sync: barrier seq={:?} up={} del={} parked={} consumed={}",
                    r.seq,
                    r.uploaded.len(),
                    r.deleted.len(),
                    r.parked.len(),
                    r.consumed
                );
            }
            Step::Sync => {
                let r = sc.sync().await?;
                println!("{}", serde_json::to_string_pretty(&r).unwrap());
            }
            Step::RecoverStaged => {
                let r = sc.recover_staged().await?;
                eprintln!(
                    "flint-sync: recover-staged seq={:?} recited={} dangling={} unrecoverable={}",
                    r.seq,
                    r.recited.len(),
                    r.dangling.len(),
                    r.unrecoverable.len()
                );
                for p in &r.recited {
                    eprintln!("flint-sync:   recited {p}");
                }
                // Named loudly: no verb can fix these — the retention
                // backstop reaped the cited version and no newer
                // generation survives.
                for p in &r.unrecoverable {
                    eprintln!("flint-sync:   UNRECOVERABLE {p}");
                }
                if !r.unrecoverable.is_empty() {
                    return Err(LeanError::State(format!(
                        "{} path(s) have no surviving version to cite",
                        r.unrecoverable.len()
                    )));
                }
            }
        }
        Ok(())
    }
}
