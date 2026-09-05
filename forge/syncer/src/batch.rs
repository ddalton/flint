//! The batch: the only path from a push to the bucket (design §4).
//!
//! One process per push, one `proc-receive` per process, and no
//! serialisation between them — so the hook decides nothing. It hands
//! its commands here and waits. Everything that must be decided once,
//! in an order, is decided here under the writer lock:
//!
//! 1. collect the pushes that arrived together;
//! 2. judge every command — staleness, fast-forward, policy — and run
//!    the `refs/for/*` merges, packing the objects they create;
//! 3. renew the lease once for the batch;
//! 4. upload every pack the bucket does not have, with its siblings;
//! 5. ONE snapshot CAS carrying every accepted ref and the pack list;
//! 6. apply the ref updates as ONE `update-ref` transaction, and only
//!    THEN report;
//! 7. derived files, and the sweep if a repack happened.
//!
//! Step 2's staleness test is the one that is easy to get wrong twice.
//! It compares the command's old-oid against BOTH the local ref and
//! the last-synced snapshot — the local ref alone would let a syncer
//! that lost a CAS accept a push against a ref the bucket has already
//! moved — and it compares against the batch's own running view, not
//! the view at collection time, because two pushes to one ref
//! routinely arrive inside a single batch window. Checking each of
//! them against the same base is exactly the defect falsifier 2
//! exists to catch: both would be told `ok` and one would be lost.

use std::collections::{BTreeMap, BTreeSet};

use super::gitcmd::{is_zero, zero_oid, MergeOutcome, RefUpdate};
use super::policy::{Policy, Verdict};
use super::{lease, packio, snapshot, ForgeError, ForgeResult, Syncer};

/// One push, as `proc-receive` handed it over.
#[derive(Debug, Clone)]
pub struct PushRequest {
    /// Correlates the report back to the waiting hook.
    pub id: u64,
    /// `REMOTE_USER` as the door verified it. Never the client's git
    /// config: a merge the server performs is authored on behalf of a
    /// principal the door authenticated.
    pub principal: String,
    /// `git push -o …` options, available because
    /// `receive.advertisePushOptions` is on.
    pub options: Vec<String>,
    pub commands: Vec<RefUpdate>,
}

impl PushRequest {
    fn strategy(&self) -> Option<&str> {
        self.options.iter().find_map(|o| o.strip_prefix("strategy=")).filter(|s| {
            // Only what `merge-tree -X` accepts. An unknown value is
            // ignored rather than passed through: git would take
            // `-Xrm -rf` as an option, and the value comes from the
            // client.
            matches!(*s, "ours" | "theirs")
        })
    }
}

/// The per-ref report, in the shape `proc-receive` relays to the
/// client. `alt_ref` carries the `option refname` line a `refs/for/*`
/// push needs: the client asked to update `refs/for/main` and what
/// actually moved is `refs/heads/main`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandResult {
    Ok {
        name: String,
        alt_ref: Option<String>,
        old_oid: Option<String>,
        new_oid: Option<String>,
    },
    Ng {
        name: String,
        reason: String,
    },
}

impl CommandResult {
    pub fn name(&self) -> &str {
        match self {
            CommandResult::Ok { name, .. } => name,
            CommandResult::Ng { name, .. } => name,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushReport {
    pub id: u64,
    pub results: Vec<CommandResult>,
}

const FOR_PREFIX: &str = "refs/for/";

/// Normalise "this ref does not exist" to one spelling. `receive-pack`
/// sends all-zeros of the repository's hash length; the snapshot omits
/// the key entirely.
fn norm(oid: Option<&str>) -> String {
    match oid {
        Some(o) if !is_zero(o) => o.to_string(),
        _ => String::new(),
    }
}

/// Run one batch. On `Ok` every push has a report; on `Err` the caller
/// must report `ng` to every push in the batch and, for a fence, stop
/// serving and exit — no path here acknowledges a push the bucket does
/// not hold.
pub async fn run_batch(
    sc: &mut Syncer,
    pushes: Vec<PushRequest>,
    policy: &Policy,
) -> ForgeResult<Vec<PushReport>> {
    sc.check_fence()?;
    if pushes.is_empty() {
        return Ok(Vec::new());
    }
    let cell = sc.cell()?.clone();
    let local = sc.git.refs().await?;

    // The agreed view: a ref the bucket and the local repository do not
    // agree about is not pushable at all until a restart reconciles
    // them. Refusing is the only safe answer — accepting against
    // either one would publish a history the other never saw.
    let mut eff: BTreeMap<String, String> = BTreeMap::new();
    let mut disagreed: BTreeSet<String> = BTreeSet::new();
    let names: BTreeSet<&String> = local.keys().chain(cell.snap.refs.keys()).collect();
    for name in names {
        let l = norm(local.get(name).map(|s| s.as_str()));
        let s = norm(cell.snap.refs.get(name).map(|s| s.as_str()));
        if l == s {
            eff.insert(name.clone(), l);
        } else {
            disagreed.insert(name.clone());
        }
    }

    let mut reports: Vec<PushReport> = Vec::new();
    let mut accepted: Vec<RefUpdate> = Vec::new();
    // Objects the SERVER created this batch (merge commits and their
    // trees). They are loose, and a pack-only upload would leave the
    // bucket holding a ref whose commit is in no pack.
    let mut merge_tips: Vec<String> = Vec::new();
    let mut merge_bases: Vec<String> = Vec::new();

    // ── step 2: judge every command, in arrival order ────────────────
    for push in &pushes {
        let mut results = Vec::new();
        for cmd in &push.commands {
            let outcome = judge(sc, push, cmd, policy, &eff, &disagreed).await;
            match outcome {
                // A git failure while JUDGING one command refuses that
                // command and nothing else. Only the steps after this
                // loop are fatal, because only they can leave the
                // bucket and the repository disagreeing. Letting a
                // merge that git would not perform take down the
                // repository server — and with it every other push in
                // the batch — is a fault amplifier, and it is what this
                // code did until the end-to-end test produced a
                // `commit-tree` error on a push with no principal.
                Err(ForgeError::Git(msg)) => {
                    results.push(CommandResult::Ng { name: cmd.name.clone(), reason: msg });
                }
                Err(e) => return Err(e),
                Ok(Judged::Refused { reason }) => {
                    results.push(CommandResult::Ng { name: cmd.name.clone(), reason });
                }
                Ok(Judged::Accepted { update, alt_ref, created }) => {
                    if let Some((tip, base)) = created {
                        merge_tips.push(tip);
                        merge_bases.push(base);
                    }
                    eff.insert(update.name.clone(), norm(Some(&update.new_oid)));
                    let new_oid = if is_zero(&update.new_oid) {
                        None
                    } else {
                        Some(update.new_oid.clone())
                    };
                    let old_oid =
                        if update.old_oid.is_empty() { None } else { Some(update.old_oid.clone()) };
                    accepted.push(update);
                    results.push(CommandResult::Ok {
                        name: cmd.name.clone(),
                        alt_ref,
                        old_oid,
                        new_oid,
                    });
                }
            }
        }
        reports.push(PushReport { id: push.id, results });
    }

    if accepted.is_empty() {
        // Nothing to publish: no lease renewal is skipped (the
        // heartbeat owns that), no CAS is spent, and every push already
        // has its report.
        return Ok(reports);
    }

    // Pack what the server itself created, before anything is uploaded.
    if !merge_tips.is_empty() {
        let mut excludes = merge_bases;
        // Excluding every ref the bucket already holds keeps the pack
        // to the objects the merge actually introduced.
        excludes.extend(cell.snap.refs.values().cloned());
        sc.git.pack_new_objects(&merge_tips, &excludes).await?;
    }

    // ── step 3: one lease renewal for the batch ──────────────────────
    lease::renew(sc).await?;

    // ── step 4: upload every pack the bucket does not have ───────────
    let local_packs = sc.git.local_packs()?;
    let known: BTreeSet<&String> = cell.snap.packs.iter().collect();
    let epoch = sc.lease()?.epoch;
    for pack in &local_packs {
        if known.contains(pack) {
            continue;
        }
        for file in sc.git.pack_siblings(pack) {
            let path = sc.git.pack_path(&file);
            packio::upload_file(sc.store.as_ref(), &sc.cfg.pack_key(&file), &path, epoch).await?;
        }
    }

    // ── step 5: ONE snapshot CAS ─────────────────────────────────────
    let mut next = cell.snap.clone();
    for u in &accepted {
        if is_zero(&u.new_oid) {
            next.refs.remove(&u.name);
        } else {
            next.refs.insert(u.name.clone(), u.new_oid.clone());
        }
    }
    next.packs = local_packs;
    // The export's commit rides this CAS rather than one of its own.
    if let Some(c) = sc.pending_exported_commit.take() {
        next.exported_commit = Some(c);
    }
    // Only the newest bundle is advertised, so only the newest is
    // named. The sweep collects the rest past the grace, which
    // comfortably outlives a clone that is already holding a URL.
    if let Some(b) = sc.pending_bundle.take() {
        next.bundles = vec![b];
    }
    let writer = sc.holder_id.clone();
    let new_cell =
        match snapshot::cas(sc.store.as_ref(), &sc.cfg, &cell, next, epoch, &writer).await {
            Ok(c) => c,
            Err(ForgeError::Store(flint_store::StoreError::PreconditionFailed(e))) => {
                // Under the writer lock this cannot be a concurrent
                // push of ours. It is a second server: a straggler
                // after a roll, or this pod after a successor rotated.
                return Err(sc.fence(format!("snapshot CAS refused, another server holds this repository: {e}")));
            }
            Err(e) => return Err(e),
        };
    sc.cell = Some(new_cell);

    // ── step 6: ONE ref transaction, THEN the reports ────────────────
    //
    // The caller sends the reports on return. A report emitted between
    // the CAS and this transaction would acknowledge refs the local
    // repository has not moved; a transaction that half-applied would
    // acknowledge a snapshot the repository does not match. Both are
    // closed by doing all of it before any of it is said.
    sc.git.update_refs(&accepted).await?;
    sc.last_push_unix = super::now_unix();

    // ── step 7: derived files, best effort, after the report path ────
    if let Err(e) = publish_derived(sc).await {
        eprintln!("flint-forge: derived files not refreshed ({e}); the smart protocol is unaffected");
    }

    Ok(reports)
}

enum Judged {
    Accepted {
        update: RefUpdate,
        alt_ref: Option<String>,
        /// `(tip, base)` when the server created objects for this
        /// command, so they can be packed before the upload.
        created: Option<(String, String)>,
    },
    Refused {
        reason: String,
    },
}

async fn judge(
    sc: &Syncer,
    push: &PushRequest,
    cmd: &RefUpdate,
    policy: &Policy,
    eff: &BTreeMap<String, String>,
    disagreed: &BTreeSet<String>,
) -> ForgeResult<Judged> {
    // Policy before mechanics, for both a direct push and a merge
    // proposal. It is the same document `pre-receive` already applied
    // at the edge; applying it again here is what makes it a guarantee
    // rather than a convention (see `policy`'s module doc). It also
    // gives the more useful message: a pusher who may not touch `main`
    // wants to be told that, not that their old-oid is stale.
    if let Verdict::Refuse(reason) = policy.judge(&push.principal, &cmd.name, &cmd.new_oid) {
        return Ok(Judged::Refused { reason });
    }
    if let Some(target) = cmd.name.strip_prefix(FOR_PREFIX) {
        return judge_merge(sc, push, cmd, target, eff, disagreed).await;
    }
    if disagreed.contains(&cmd.name) {
        return Ok(Judged::Refused {
            reason: format!(
                "{} differs between this server and the bucket; the server will reconcile on \
                 restart — retry then",
                cmd.name
            ),
        });
    }
    let base = eff.get(&cmd.name).cloned().unwrap_or_default();
    let old = norm(Some(&cmd.old_oid));
    if old != base {
        // The message git clients already know how to explain.
        return Ok(Judged::Refused {
            reason: "stale info: fetch first".into(),
        });
    }
    let deleting = is_zero(&cmd.new_oid);
    if !deleting && !base.is_empty() {
        let ff = sc.git.is_ancestor(&base, &cmd.new_oid).await?;
        if !ff && !policy.allows_non_fast_forward(&cmd.name) {
            return Ok(Judged::Refused {
                reason: format!("non-fast-forward update to {}", cmd.name),
            });
        }
    }
    if !deleting && !sc.git.has_object(&cmd.new_oid).await? {
        // The pack did not carry what the command names. git's own
        // `receive.fsckObjects` refuses most of this at the door; this
        // closes the rest, before an oid with no object reaches the
        // bucket.
        return Ok(Judged::Refused {
            reason: format!("{} is not in this repository", cmd.new_oid),
        });
    }
    Ok(Judged::Accepted {
        update: RefUpdate {
            name: cmd.name.clone(),
            old_oid: base,
            new_oid: cmd.new_oid.clone(),
        },
        alt_ref: None,
        created: None,
    })
}

/// A push to `refs/for/<target>` proposes a merge (design §6): the
/// AGit flow Gitea and Gitee run, so there is no merge API, no second
/// authenticated surface, and exactly one path to the bucket.
async fn judge_merge(
    sc: &Syncer,
    push: &PushRequest,
    cmd: &RefUpdate,
    target: &str,
    eff: &BTreeMap<String, String>,
    disagreed: &BTreeSet<String>,
) -> ForgeResult<Judged> {
    let target_ref = if target.starts_with("refs/") {
        target.to_string()
    } else {
        format!("refs/heads/{target}")
    };
    if disagreed.contains(&target_ref) {
        return Ok(Judged::Refused {
            reason: format!("{target_ref} differs between this server and the bucket; retry after \
                             the server reconciles"),
        });
    }
    let base = eff.get(&target_ref).cloned().unwrap_or_default();
    let head = cmd.new_oid.clone();
    if is_zero(&head) {
        return Ok(Judged::Refused { reason: "a merge request must name a commit".into() });
    }
    if base.is_empty() {
        // BOOTSTRAP. A new repository has no default branch, and the
        // two ways to make one both refuse: a direct push because the
        // branch is protected, and a merge request because there is
        // nothing to merge into. Between them, main could never be
        // created and the repository was unusable from birth — which
        // is exactly what the first cluster run found.
        //
        // Creating it here is within the authority already checked:
        // `mergeInto` named this principal as one who may move this
        // ref, and moving it from nothing is the smallest such move.
        // Narrow on purpose — only the DEFAULT branch, so a merge
        // request cannot be used to conjure arbitrary refs.
        let default_ref = format!("refs/heads/{}", sc.cfg.default_branch);
        if target_ref != default_ref {
            return Ok(Judged::Refused {
                reason: format!("no such merge target: {target_ref}"),
            });
        }
        if !sc.git.has_object(&head).await? {
            return Ok(Judged::Refused { reason: format!("{head} is not in this repository") });
        }
        return Ok(Judged::Accepted {
            update: RefUpdate {
                name: target_ref.clone(),
                // A creation, which `update_refs` spells with a zero
                // old oid — NOT the empty string `eff` uses for absent.
                old_oid: zero_oid(head.len()),
                new_oid: head,
            },
            alt_ref: Some(target_ref),
            created: None,
        });
    }
    if !sc.git.has_object(&head).await? {
        return Ok(Judged::Refused { reason: format!("{head} is not in this repository") });
    }
    // Already contained: nothing to do, and saying so is not a failure.
    if head == base || sc.git.is_ancestor(&head, &base).await? {
        return Ok(Judged::Refused {
            reason: format!("{head} is already contained in {target_ref}"),
        });
    }
    // Fast-forward: no merge commit, and no objects created.
    if sc.git.is_ancestor(&base, &head).await? {
        return Ok(Judged::Accepted {
            update: RefUpdate {
                name: target_ref.clone(),
                old_oid: base,
                new_oid: head.clone(),
            },
            alt_ref: Some(target_ref),
            created: None,
        });
    }
    match sc.git.merge_tree(&base, &head, push.strategy()).await? {
        MergeOutcome::Conflict { paths } => Ok(Judged::Refused {
            reason: format!(
                "conflict: {}",
                if paths.is_empty() { "the merge does not apply".to_string() } else { paths.join(" ") }
            ),
        }),
        MergeOutcome::Clean { tree } => {
            let msg = format!("Merge {head} into {target_ref}");
            let commit = sc
                .git
                .commit_tree(&tree, &[base.clone(), head.clone()], &msg, &push.principal)
                .await?;
            Ok(Judged::Accepted {
                update: RefUpdate {
                    name: target_ref.clone(),
                    old_oid: base.clone(),
                    new_oid: commit.clone(),
                },
                alt_ref: Some(target_ref),
                created: Some((commit, base)),
            })
        }
    }
}

/// The dumb protocol's derived files, so the bucket alone is a
/// read-only remote (design §3).
///
/// `objects/info/packs` goes up BEFORE `info/refs`. A dumb clone that
/// reads fresh refs against a stale pack list looks for objects that
/// are not listed and fails; the reverse order serves the previous
/// state, which is merely old. `git update-server-info` writes both
/// files in the repository, so the format is git's rather than ours.
async fn publish_derived(sc: &mut Syncer) -> ForgeResult<()> {
    sc.git.must(&["update-server-info"], None).await?;
    let epoch = sc.lease()?.epoch;
    let repo = sc.cfg.repo.clone();
    for (rel, key) in [
        ("objects/info/packs", sc.cfg.info_packs_key()),
        ("info/refs", sc.cfg.info_refs_key()),
    ] {
        let path = repo.join(rel);
        if path.exists() {
            let body = std::fs::read(&path)?;
            packio::put_small(sc.store.as_ref(), &key, body, epoch).await?;
        }
    }
    let head = sc.git.head_target().await?;
    packio::put_small(
        sc.store.as_ref(),
        &sc.cfg.head_key(),
        format!("ref: {head}\n").into_bytes(),
        epoch,
    )
    .await?;
    Ok(())
}
