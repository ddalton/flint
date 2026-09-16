//! The one-shot verbs, and which of them holds the publish fence for
//! its whole run.
//!
//! Lifted out of `bin/flint_sync.rs` so the RULE — which verb claims —
//! is a property the battery can execute rather than a line of dispatch
//! nobody can reach from a test. `a_checkout_does_not_wait_out_a_\
//! standing_lease` in `tests.rs` calls `lease_free_then`, the same
//! function the dispatch calls, rather than a re-implementation of it.

use super::lease;
use super::{LeanError, Syncer};

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
    /// The narrow/widen verb. `None` is the whole tree; an empty
    /// argument list therefore cannot mean "narrow to nothing".
    Rescope(Option<Vec<String>>),
}

impl Step {
    /// Does this verb hold the publish fence for its WHOLE run?
    ///
    /// The epoch cell arbitrates who may INSTALL a manifest, and since
    /// the lease is held per barrier (design 2026-09-13 §4) a barrier
    /// claims it INSIDE — after its uploads, for the commit section
    /// only — so the `barrier` verb needs no outer claim. `checkout`
    /// and `sync` install nothing: every GET they make carries the
    /// cited etag as `If-Match`, so a publisher racing them yields a
    /// handled 412, never a torn file, and holding the fence around
    /// them only ever serialised readers on one cell (four syncers
    /// reading disjoint subtrees ran 2.1x faster without it, 0.41x with
    /// it — measured 2026-09-11).
    ///
    /// `rescope` is the only `true`. It reads the bucket and writes
    /// nothing to it either, but it rewrites the held set and leaves a
    /// replayable intent behind, so it stays fenced until someone has
    /// walked that crash matrix with two of them running.
    pub fn holds_the_fence_throughout(&self) -> bool {
        match self {
            Step::Rescope(_) => true,
            Step::Checkout | Step::Barrier | Step::Sync => false,
        }
    }
}

/// The one door every one-shot verb goes through. The ROUTING lives
/// here rather than in the binary's argv match so that the battery
/// exercises the same decision the shipped binary makes.
pub async fn run_verb(sc: &mut Syncer, step: Step) -> Result<(), LeanError> {
    // A reader publishes nothing and holds no fence. `rescope` writes
    // nothing either, but it claims the fence for its whole run, and a
    // reader's credential cannot.
    match &step {
        Step::Barrier => sc.refuse_if_read("barrier")?,
        Step::Rescope(_) => sc.refuse_if_read("rescope")?,
        Step::Checkout | Step::Sync => {}
    }
    if step.holds_the_fence_throughout() {
        claim_then(sc, step).await
    } else {
        lease_free_then(sc, step).await
    }
}

/// Take the fence, do one step, release it.
pub async fn claim_then(sc: &mut Syncer, step: Step) -> Result<(), LeanError> {
    lease::verify_claim(sc).await?;
    lease::warn_if_prefix_is_shared(sc).await;
    let held = lease::claim(sc).await?;
    eprintln!("flint-sync: holding epoch {}", held.epoch);
    let out = run_step(sc, step).await;
    if let Err(e) = lease::release(sc).await {
        eprintln!("flint-sync: the publish fence could not be released ({e}); a waiter deposes it");
    }
    out
}

/// A verb that claims nothing up front. Two things here are not the
/// lease and stay:
///   * `verify_claim` — the project-id precondition. A refusal, not a
///     fence: this prefix is another project's and must not be read or
///     written by us at all.
///   * `warn_if_prefix_is_shared` — a diagnostic nobody else emits.
pub async fn lease_free_then(sc: &mut Syncer, step: Step) -> Result<(), LeanError> {
    lease::verify_claim(sc).await?;
    lease::warn_if_prefix_is_shared(sc).await;
    run_step(sc, step).await
}

/// The verb bodies. Shared by both doors above so that the ONLY
/// difference between a fenced verb and a lease-free one is the door,
/// never a second copy of the work.
pub async fn run_step(sc: &mut Syncer, step: Step) -> Result<(), LeanError> {
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
                    "flint-sync: barrier seq={:?} up={} del={} parked={} consumed={}{}",
                    r.seq,
                    r.uploaded.len(),
                    r.deleted.len(),
                    r.parked.len(),
                    r.consumed,
                    // Only on a store whose collector had to give way
                    // (`conformance.rs`), so the ordinary line stays
                    // byte-identical for whatever greps it. `del` counts
                    // objects COLLECTED; these left the boundary and
                    // stayed in the bucket, and a barrier that said
                    // nothing about them would read as a tidy delete.
                    if r.leaked.is_empty() {
                        String::new()
                    } else {
                        format!(" leaked={}", r.leaked.len())
                    }
                );
            }
            Step::Sync => {
                let r = sc.sync().await?;
                println!("{}", serde_json::to_string_pretty(&r).unwrap());
            }
        }
        Ok(())
    }
}
