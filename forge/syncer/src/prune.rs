//! Pruning agent branches (design §7, §8).
//!
//! A thousand one-commit agent branches cost every clone: 0.54 CPU-s
//! instead of 0.13, and a 74 KB ref advertisement on every request
//! (measured). `--single-branch` is the client's half of the answer;
//! this is the server's.
//!
//! **Age alone is not the rule, and that is the point.** An agent
//! branch that was never merged is somebody's unfinished work, and
//! deleting it by a clock would be losing it — the objects survive
//! until the next repack, but nothing names them and nobody knows to
//! look. So a branch is pruned only when BOTH hold: its tip is already
//! contained in the integration branch, so nothing is lost that `main`
//! does not have; and it has been quiet longer than the TTL, so a
//! merge that just landed does not take the branch out from under the
//! agent still pushing to it.
//!
//! It is off unless configured, and the deletions go through the
//! ordinary batch — one snapshot CAS, one ref transaction — because a
//! ref this process moves outside that path is a ref the bucket does
//! not know about.

use super::gitcmd::{zero_oid, RefUpdate};
use super::policy::glob_match;
use super::{ForgeResult, Syncer};

#[derive(Debug, Clone)]
pub struct PruneConfig {
    /// Which refs are prunable at all, e.g. `refs/heads/agent/*`.
    pub pattern: String,
    /// How long a merged branch must have been quiet.
    pub after_secs: u64,
    /// The branch a candidate must already be contained in.
    pub into: String,
    /// How often the pass runs.
    pub every_secs: u64,
}

/// Which branches are prunable right now.
///
/// Returns deletions in the shape a push has, so they travel the same
/// path a client's would: the same staleness check, the same CAS, the
/// same transaction.
pub async fn candidates(
    sc: &Syncer,
    cfg: &PruneConfig,
    now: u64,
) -> ForgeResult<Vec<RefUpdate>> {
    let Some(into) = sc.git.ref_oid(&cfg.into).await? else {
        // No integration branch means nothing is contained in it, so
        // nothing is safe to prune. Refusing to prune is the right
        // failure direction every time.
        return Ok(Vec::new());
    };
    let listing = sc
        .git
        .must(&["for-each-ref", "--format=%(objectname) %(committerdate:unix) %(refname)"], None)
        .await?;

    let mut out = Vec::new();
    for line in listing.lines() {
        let mut parts = line.splitn(3, ' ');
        let (Some(oid), Some(when), Some(name)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if !glob_match(&cfg.pattern, name) {
            continue;
        }
        let when: u64 = match when.parse() {
            Ok(w) => w,
            // A ref whose date we cannot read is a ref we do not touch.
            Err(_) => continue,
        };
        if now.saturating_sub(when) < cfg.after_secs {
            continue;
        }
        // Contained in the integration branch: nothing is lost that
        // `into` does not already have.
        if !sc.git.is_ancestor(oid, &into).await? {
            continue;
        }
        out.push(RefUpdate {
            name: name.to_string(),
            old_oid: oid.to_string(),
            new_oid: zero_oid(oid.len()),
        });
    }
    Ok(out)
}
