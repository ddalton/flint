//! The file API's verbs over the bare repository
//! (`docs/plans/forge-file-api-design.md`).
//!
//! Six verbs with flint lite's shape — `?path=`, `If-Match`, the same
//! status codes — so one client speaks to a lite share and a forge
//! repository without knowing which it has. What differs is deliberate
//! and recorded in §3.5 of the design; what is identical is identical
//! because a browser file manager should not have to care.
//!
//! **This module plans; it does not write.** Every mutating verb
//! returns a [`WritePlan`] carrying a tree oid. Turning that into a
//! commit and moving a ref belongs to the batch, which is the syncer's
//! one path to the bucket — a handler that moved a ref itself would be
//! a second writer, and the ref would be one the bucket never learns
//! about. Object construction is safe to do here because it writes only
//! unreachable loose objects.

use super::gitcmd::{Git, IndexEdit, TreeEntry};
use super::ForgeError;

/// The largest object this API will read or write in one request.
///
/// Not a performance knob — a memory bound. The syncer's container is
/// sized for `git http-backend` (25m/32Mi) and this listener buffers,
/// so the cap is what makes buffering safe. It is enforceable BEFORE
/// any allocation because `ls-tree` reports a blob's size without
/// reading it. Agents move large objects with a real git client
/// through the git door, which streams.
pub const DEFAULT_MAX_BYTES: u64 = 10 * 1024 * 1024;

/// The most paths one directory rename may rewrite.
///
/// A directory rename copies no CONTENT — every blob keeps its oid and
/// only its name moves, which is why this operation is cheap in a way
/// the same rename on S3 (a COPY of every byte, per object) is not.
/// What it does cost is one index line per file, so the bound is on
/// the count. Above it the answer is a refusal with a reason rather
/// than a request that quietly takes minutes.
pub const MAX_RENAME_ENTRIES: usize = 10_000;

/// Every way a file request can fail, with the wire answer attached.
///
/// The taxonomy IS the specification: the application branches on
/// `reason` and shows `message`, so `message` is written for a person
/// and never echoes git's own phrasing at them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileError {
    /// The path could never be valid — traversal, absolute, `.git`.
    BadPath(String),
    /// No such path at this revision.
    NotFound,
    /// The path is a directory and the caller asked for content.
    IsADirectory,
    /// A symlink or a submodule. Neither is servable as file bytes: a
    /// symlink's content is its target, and a gitlink names a commit
    /// that is not in this repository.
    NotAFile(&'static str),
    /// A write whose target is a directory. Refused rather than
    /// performed: git would REPLACE the whole subtree, silently.
    WouldReplaceDirectory,
    /// A directory delete, which in git is always recursive because a
    /// directory only exists while it holds a file.
    WouldDeleteDirectory,
    /// A directory rename over [`MAX_RENAME_ENTRIES`] paths.
    TooManyEntries { count: usize, cap: usize },
    /// Over [`DEFAULT_MAX_BYTES`].
    TooLarge { size: u64, cap: u64 },
    /// The path exists and the caller sent no `If-Match`. Refusing an
    /// unconditioned overwrite is what makes many writers safe.
    PreconditionRequired,
    /// `If-Match` did not match. The caller's copy is stale.
    FileChanged { etag: String },
    /// The repository has no commits yet.
    Unborn,
    /// The ref moved because a DIFFERENT path was written, and the
    /// bounded retry did not converge. **Not a conflict on this
    /// caller's file** — telling them to refresh would be wrong advice,
    /// because refreshing shows them nothing different.
    RefContended,
    /// The policy refuses this principal on this ref.
    PolicyRefused(String),
    /// The repository and the bucket disagree about the ref; the server
    /// reconciles on restart.
    Reconciling(String),
    /// Anything git said that this module did not anticipate.
    Git(String),
}

impl FileError {
    pub fn status(&self) -> u16 {
        match self {
            FileError::BadPath(_) => 400,
            FileError::NotFound | FileError::Unborn => 404,
            FileError::IsADirectory
            | FileError::NotAFile(_)
            | FileError::WouldReplaceDirectory
            | FileError::WouldDeleteDirectory => 409,
            FileError::TooManyEntries { .. } => 413,
            FileError::TooLarge { .. } => 413,
            FileError::PreconditionRequired => 428,
            FileError::FileChanged { .. } => 412,
            FileError::RefContended => 409,
            FileError::PolicyRefused(_) => 403,
            FileError::Reconciling(_) => 503,
            FileError::Git(_) => 500,
        }
    }

    /// The machine-readable half. Stable; the message is not.
    pub fn reason(&self) -> &'static str {
        match self {
            FileError::BadPath(_) => "bad-path",
            FileError::NotFound => "not-found",
            FileError::Unborn => "empty-repository",
            FileError::IsADirectory => "is-a-directory",
            FileError::NotAFile(_) => "not-a-file",
            FileError::WouldReplaceDirectory => "would-replace-directory",
            FileError::WouldDeleteDirectory => "would-delete-directory",
            FileError::TooManyEntries { .. } => "too-many-entries",
            FileError::TooLarge { .. } => "too-large",
            FileError::PreconditionRequired => "precondition-required",
            FileError::FileChanged { .. } => "file-changed",
            FileError::RefContended => "ref-contended",
            FileError::PolicyRefused(_) => "policy-refused",
            FileError::Reconciling(_) => "reconciling",
            FileError::Git(_) => "internal",
        }
    }

    pub fn message(&self) -> String {
        match self {
            FileError::BadPath(why) => format!("the path is not usable: {why}"),
            FileError::NotFound => "no such file or directory".into(),
            FileError::Unborn => "this repository has no commits yet".into(),
            FileError::IsADirectory => "path is a directory; list it with GET /files".into(),
            FileError::NotAFile(k) => {
                format!("path is a {k}; it has no file content to read or write")
            }
            FileError::WouldReplaceDirectory => {
                "path is a directory; writing a file here would delete everything under it".into()
            }
            FileError::WouldDeleteDirectory => {
                "path is a directory; delete the files under it instead".into()
            }
            FileError::TooManyEntries { count, cap } => format!(
                "this would rename {count} files and the limit is {cap}; move them in smaller \
                 groups, or use a git client"
            ),
            FileError::TooLarge { size, cap } => format!(
                "the file is {size} bytes and this API carries {cap}; clone the repository to \
                 work with it"
            ),
            FileError::PreconditionRequired => {
                "the file already exists; send If-Match with the version you read".into()
            }
            FileError::FileChanged { .. } => {
                "the file changed since you read it; re-read it and try again".into()
            }
            FileError::RefContended => "the repository is busy with other writes; try again"
                .into(),
            FileError::PolicyRefused(why) => format!("this write is not permitted: {why}"),
            FileError::Reconciling(why) => {
                format!("the repository is reconciling and cannot accept writes yet: {why}")
            }
            FileError::Git(e) => format!("the repository could not answer: {e}"),
        }
    }
}

impl From<ForgeError> for FileError {
    fn from(e: ForgeError) -> Self {
        match e {
            // `build_tree` refuses a path git would silently drop.
            ForgeError::Refused(m) => FileError::BadPath(m),
            other => FileError::Git(other.to_string()),
        }
    }
}

pub type FileResult<T> = Result<T, FileError>;

/// One entry as the API renders it. `etag` is the object id — content
/// addressed, so unlike a synthetic validator it cannot drift when
/// nothing changed.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileEntry {
    pub name: String,
    pub path: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// `None` for directories and submodules, which have no size.
    pub size: Option<u64>,
    pub mode: String,
    pub etag: String,
}

impl FileEntry {
    fn of(e: &TreeEntry, full_path: &str) -> Self {
        FileEntry {
            name: full_path.rsplit('/').next().unwrap_or(full_path).to_string(),
            path: full_path.to_string(),
            kind: e.kind_name(),
            size: e.size,
            mode: e.mode.clone(),
            etag: e.oid.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Listing {
    pub path: String,
    pub entries: Vec<FileEntry>,
    /// The tree's own oid. A listing ETag, which lite's file API has no
    /// way to offer — a polling file manager can revalidate a directory
    /// for the cost of one request that returns nothing.
    pub etag: String,
}

/// A mutation, planned but not performed. `tree` is what the caller
/// commits; `etag` is what the caller returns to the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WritePlan {
    pub tree: String,
    pub etag: String,
}

/// Reject a path before git sees it, so the answer is a 400 with a
/// reason rather than whatever git chose to say.
fn check(path: &str) -> FileResult<()> {
    super::gitcmd::validate_tree_path(path).map_err(FileError::BadPath)
}

/// The tree oid at `rev`, or `None` when the branch is unborn.
pub async fn tree_of(git: &Git, rev: &str) -> FileResult<Option<String>> {
    match git.ref_oid(rev).await.map_err(FileError::from)? {
        None => Ok(None),
        Some(commit) => {
            let spec = format!("{commit}^{{tree}}");
            let out = git.run_bytes(&["rev-parse", "--verify", "--quiet", &spec], &[], None)
                .await
                .map_err(FileError::from)?;
            if out.status != 0 {
                return Ok(None);
            }
            Ok(Some(String::from_utf8_lossy(&out.stdout).trim().to_string()))
        }
    }
}

/// One directory page. `path` empty means the root.
pub async fn list(git: &Git, tree: Option<&str>, path: &str) -> FileResult<Listing> {
    let Some(tree) = tree else { return Err(FileError::Unborn) };
    if !path.is_empty() {
        check(path)?;
        // A file is not a listing, and neither is a symlink. Answer the
        // same way lite does rather than returning the single entry.
        match git.tree_entry(tree, path).await.map_err(FileError::from)? {
            None => return Err(FileError::NotFound),
            Some(e) if e.kind_name() != "directory" => {
                return Err(FileError::NotAFile("file"))
            }
            Some(_) => {}
        }
    }
    let entries = git.ls_tree(tree, path, false).await.map_err(FileError::from)?;
    let dir_oid = if path.is_empty() {
        tree.to_string()
    } else {
        git.tree_entry(tree, path)
            .await
            .map_err(FileError::from)?
            .map(|e| e.oid)
            .ok_or(FileError::NotFound)?
    };
    let prefix = if path.is_empty() { String::new() } else { format!("{path}/") };
    Ok(Listing {
        path: path.to_string(),
        // `ls-tree <tree>:<dir>` reports names relative to the
        // directory; the API answers with paths from the root.
        entries: entries
            .iter()
            .map(|e| FileEntry::of(e, &format!("{prefix}{}", e.path)))
            .collect(),
        etag: dir_oid,
    })
}

/// One entry's metadata, with every kind refusal the read path owes.
pub async fn stat(git: &Git, tree: Option<&str>, path: &str) -> FileResult<(TreeEntry, FileEntry)> {
    let Some(tree) = tree else { return Err(FileError::Unborn) };
    check(path)?;
    let e = git
        .tree_entry(tree, path)
        .await
        .map_err(FileError::from)?
        .ok_or(FileError::NotFound)?;
    match e.kind_name() {
        "directory" => Err(FileError::IsADirectory),
        "symlink" => Err(FileError::NotAFile("symbolic link")),
        "submodule" => Err(FileError::NotAFile("submodule")),
        _ => {
            let rendered = FileEntry::of(&e, path);
            Ok((e, rendered))
        }
    }
}

/// A file's bytes. The cap is checked against the size in the TREE, so
/// an oversized object is refused without being read.
pub async fn read(
    git: &Git,
    tree: Option<&str>,
    path: &str,
    cap: u64,
) -> FileResult<(FileEntry, Vec<u8>)> {
    let (raw, rendered) = stat(git, tree, path).await?;
    if let Some(size) = raw.size {
        if size > cap {
            return Err(FileError::TooLarge { size, cap });
        }
    }
    let bytes = git.cat_blob(&raw.oid).await.map_err(FileError::from)?;
    Ok((rendered, bytes))
}

/// Decide whether a conditional write may proceed against `existing`.
///
/// The rule that makes many writers safe: an existing path REQUIRES an
/// `If-Match`. Without it two people editing one file in a browser
/// silently overwrite each other, and neither is told.
fn judge(existing: Option<&TreeEntry>, if_match: Option<&str>) -> FileResult<()> {
    match (existing, if_match) {
        (Some(e), Some(tag)) => {
            let tag = tag.trim().trim_matches('"');
            if tag == "*" || tag == e.oid {
                Ok(())
            } else {
                Err(FileError::FileChanged { etag: e.oid.clone() })
            }
        }
        (Some(_), None) => Err(FileError::PreconditionRequired),
        // Creating. `If-Match` on a path that is not there is a stale
        // caller: it read a file that has since been deleted.
        (None, Some(tag)) if tag.trim().trim_matches('"') != "*" => {
            Err(FileError::FileChanged { etag: String::new() })
        }
        (None, _) => Ok(()),
    }
}

/// Plan a create-or-update.
pub async fn plan_put(
    git: &Git,
    tree: Option<&str>,
    path: &str,
    body: &[u8],
    if_match: Option<&str>,
    cap: u64,
) -> FileResult<WritePlan> {
    check(path)?;
    if body.len() as u64 > cap {
        return Err(FileError::TooLarge { size: body.len() as u64, cap });
    }
    let existing = match tree {
        Some(t) => git.tree_entry(t, path).await.map_err(FileError::from)?,
        None => None,
    };
    if let Some(e) = &existing {
        match e.kind_name() {
            // The guard for the silent recursive delete: git would
            // accept this write and remove the whole subtree.
            "directory" => return Err(FileError::WouldReplaceDirectory),
            "symlink" => return Err(FileError::NotAFile("symbolic link")),
            "submodule" => return Err(FileError::NotAFile("submodule")),
            _ => {}
        }
    }
    judge(existing.as_ref(), if_match)?;

    // The mode is read, never assumed: naming the wrong one silently
    // moves the executable bit.
    let mode = existing.as_ref().map(|e| e.mode.clone()).unwrap_or_else(|| "100644".into());
    let oid = git.hash_object(body).await.map_err(FileError::from)?;
    let next = git
        .build_tree(tree, &[IndexEdit::Set { path: path.into(), mode, oid: oid.clone() }])
        .await
        .map_err(FileError::from)?;
    Ok(WritePlan { tree: next, etag: oid })
}

/// Plan a delete. A directory is refused: in git it is never empty, so
/// deleting one is always recursive.
pub async fn plan_delete(
    git: &Git,
    tree: Option<&str>,
    path: &str,
    if_match: Option<&str>,
) -> FileResult<WritePlan> {
    let Some(t) = tree else { return Err(FileError::Unborn) };
    check(path)?;
    let existing = git.tree_entry(t, path).await.map_err(FileError::from)?;
    let Some(e) = existing else { return Err(FileError::NotFound) };
    if e.kind_name() == "directory" {
        return Err(FileError::WouldDeleteDirectory);
    }
    judge(Some(&e), if_match)?;
    let next = git
        .build_tree(Some(t), &[IndexEdit::Remove { path: path.into() }])
        .await
        .map_err(FileError::from)?;
    Ok(WritePlan { tree: next, etag: e.oid })
}

/// Plan a rename or move — one tree edit, so it lands or it does not.
///
/// `If-Match` conditions the SOURCE when given, matching lite: the
/// thing being moved is the thing the caller read. Unlike `PUT` and
/// `DELETE` it is **not required**, and the difference is principled
/// rather than a concession: those two destroy content, so an
/// unconditioned one silently loses somebody's work. A rename destroys
/// nothing — the content moves with the name, whatever it has become
/// since the caller looked.
///
/// The destination is checked for existence separately and always,
/// because a move that silently replaced a file would be the same
/// data-loss hazard in a politer costume.
pub async fn plan_move(
    git: &Git,
    tree: Option<&str>,
    from: &str,
    to: &str,
    if_match: Option<&str>,
) -> FileResult<WritePlan> {
    let Some(t) = tree else { return Err(FileError::Unborn) };
    check(from)?;
    check(to)?;
    if from == to {
        return Err(FileError::BadPath("the source and destination are the same".into()));
    }
    // Moving a directory INTO itself would build a tree that cannot be
    // expressed; refuse before git tries.
    if to.starts_with(&format!("{from}/")) {
        return Err(FileError::BadPath("the destination is inside the source".into()));
    }
    let src = git
        .tree_entry(t, from)
        .await
        .map_err(FileError::from)?
        .ok_or(FileError::NotFound)?;
    // Honoured when given, not required. A caller that sends a stale
    // one is refused: silently ignoring a condition the caller thought
    // it was protected by is worse than not offering the condition at
    // all. (A directory has no single version, so it is exempt — the
    // branch below conditions on nothing and says so.)
    if if_match.is_some() && src.kind_name() != "directory" {
        judge(Some(&src), if_match)?;
    }
    if let Some(dst) = git.tree_entry(t, to).await.map_err(FileError::from)? {
        return Err(match dst.kind_name() {
            "directory" => FileError::WouldReplaceDirectory,
            _ => FileError::FileChanged { etag: dst.oid },
        });
    }

    // A DIRECTORY rename moves names, never content: every blob keeps
    // its oid and only the trees above it change. That makes it cheap
    // here in a way the same operation is not on S3, where renaming a
    // folder copies every byte of every object under it. It is refused
    // only for size, and it is still ONE commit — so it lands whole or
    // not at all, which S3 also cannot offer.
    if src.kind_name() == "directory" {
        // A directory has no single version to condition on. The
        // caller conditions on its contents by having read them.
        let under = git.ls_tree(t, from, true).await.map_err(FileError::from)?;
        if under.len() > MAX_RENAME_ENTRIES {
            return Err(FileError::TooManyEntries {
                count: under.len(),
                cap: MAX_RENAME_ENTRIES,
            });
        }
        let mut edits = Vec::with_capacity(under.len() * 2);
        for e in &under {
            // `ls_tree` reports paths relative to `from`.
            let old_path = format!("{from}/{}", e.path);
            let new_path = format!("{to}/{}", e.path);
            check(&new_path)?;
            edits.push(IndexEdit::Remove { path: old_path });
            edits.push(IndexEdit::Set {
                path: new_path,
                mode: e.mode.clone(),
                oid: e.oid.clone(),
            });
        }
        let next = git.build_tree(Some(t), &edits).await.map_err(FileError::from)?;
        return Ok(WritePlan { tree: next, etag: src.oid });
    }

    let next = git
        .build_tree(
            Some(t),
            &[
                IndexEdit::Remove { path: from.into() },
                IndexEdit::Set { path: to.into(), mode: src.mode.clone(), oid: src.oid.clone() },
            ],
        )
        .await
        .map_err(FileError::from)?;
    Ok(WritePlan { tree: next, etag: src.oid })
}


// ── Applying a plan: the batch is the only path to the bucket ──────────

/// How many times a write may rebuild onto a moved tip before giving up.
///
/// Almost never used, and kept anyway. Writes are planned INSIDE the
/// serving loop, which is the only thing that moves this repository's
/// refs and is single-threaded over `&mut Syncer` — so the tip cannot
/// move between the plan and the CAS. What is left is the case the
/// batch calls "differs between this server and the bucket", which a
/// restart reconciles and a retry cannot.
pub const MAX_REF_RETRIES: usize = 2;

/// A mutation the caller asked for, before it is planned against any
/// particular tree. Held as data so the loop can plan it against the
/// tip it is actually going to commit onto.
#[derive(Debug, Clone)]
pub enum Mutation {
    Put { path: String, body: Vec<u8>, if_match: Option<String> },
    Delete { path: String, if_match: Option<String> },
    Move { from: String, to: String, if_match: Option<String> },
}

impl Mutation {
    pub fn summary(&self) -> String {
        match self {
            Mutation::Put { path, .. } => format!("write {path}"),
            Mutation::Delete { path, .. } => format!("delete {path}"),
            Mutation::Move { from, to, .. } => format!("move {from} to {to}"),
        }
    }
}

/// One caller's write, handed to the serving loop with somewhere to
/// send the verdict.
///
/// The handler never touches the bucket and never moves a ref: a ref
/// this process moved outside the batch would be a ref the bucket does
/// not know about. It does not build the tree either — see
/// [`run_writes`] for why that is the loop's job and not the
/// handler's.
#[derive(Debug)]
pub struct FileWrite {
    pub mutation: Mutation,
    /// The end user, from `X-Flint-Author`. Untrusted and recorded —
    /// git's author field, exactly as a laptop's `user.name` is.
    pub author: String,
    /// The door-verified identity. What the policy judges, and the
    /// commit's committer.
    pub principal: String,
    pub cap: u64,
    pub reply: tokio::sync::oneshot::Sender<FileResult<String>>,
}

/// The tree of a specific COMMIT — not of a ref.
///
/// The distinction is a safety argument, not a convenience: reading the
/// ref again would let the tree come from a different commit than the
/// one the write is parented on, and the ref CAS would still pass
/// because the PARENT is right. That combination loses the other
/// writer's file with no error anywhere. Measured during the design.
pub async fn tree_of_commit(git: &Git, commit: &str) -> FileResult<Option<String>> {
    let spec = format!("{commit}^{{tree}}");
    let out = git
        .run_bytes(&["rev-parse", "--verify", "--quiet", &spec], &[], None)
        .await
        .map_err(FileError::from)?;
    if out.status != 0 {
        return Ok(None);
    }
    Ok(Some(String::from_utf8_lossy(&out.stdout).trim().to_string()))
}

/// Map a batch refusal onto the taxonomy. The strings are the ones
/// `batch::judge_one` produces; anything unrecognised stays a 500
/// rather than being guessed at.
pub fn classify(reason: &str) -> FileError {
    if reason.contains("stale info") {
        FileError::RefContended
    } else if reason.contains("differs between this server and the bucket") {
        FileError::Reconciling(reason.to_string())
    } else {
        FileError::PolicyRefused(reason.to_string())
    }
}

/// Plan one mutation against `tree`, returning the plan.
async fn plan_one(
    git: &Git,
    tree: Option<&str>,
    m: &Mutation,
    cap: u64,
) -> FileResult<WritePlan> {
    match m {
        Mutation::Put { path, body, if_match } => {
            plan_put(git, tree, path, body, if_match.as_deref(), cap).await
        }
        Mutation::Delete { path, if_match } => {
            plan_delete(git, tree, path, if_match.as_deref()).await
        }
        Mutation::Move { from, to, if_match } => {
            plan_move(git, tree, from, to, if_match.as_deref()).await
        }
    }
}

/// What a batch of file writes produced: one ref movement, and a
/// verdict per caller.
pub struct WriteBatch {
    pub command: Option<super::gitcmd::RefUpdate>,
    /// `(reply, verdict)` in arrival order. A caller refused during
    /// planning is answered here and is NOT in the commit chain.
    pub verdicts: Vec<(tokio::sync::oneshot::Sender<FileResult<String>>, FileResult<String>)>,
}

/// Plan a batch of writes into ONE ref movement, keeping one commit per
/// caller so that each keeps their own authorship.
///
/// **Why the loop and not the handler.** The serving loop is the only
/// thing that moves this repository's refs and it is single-threaded
/// over `&mut Syncer`, so a tree planned here cannot go stale before
/// the CAS. Planning in the handler instead would make ref contention
/// the common case with many writers — every concurrent save would
/// collide on the ref and retry — for no gain.
///
/// **Why a chain and not one squashed commit.** Several people's saves
/// arriving together must not become one commit with one name on it.
/// Each write is committed onto the previous, and the single
/// `RefUpdate` moves the ref to the last — so authorship survives and
/// the whole burst still costs one CAS.
///
/// A caller whose write is refused (a stale `If-Match`, a directory, a
/// path git will not take) is answered immediately and does not break
/// the chain for anyone else.
pub async fn plan_writes(git: &Git, branch: &str, writes: Vec<FileWrite>) -> WriteBatch {
    // Never returns an error: every caller in `writes` holds a reply
    // channel, and a function that could fail before answering them
    // would hang a browser tab. A top-level failure becomes everyone's
    // verdict.
    let tip = match git.ref_oid(branch).await {
        Ok(t) => t,
        Err(e) => {
            let fe = FileError::from(e);
            return WriteBatch {
                command: None,
                verdicts: writes.into_iter().map(|w| (w.reply, Err(fe.clone()))).collect(),
            };
        }
    };
    let mut head = tip.clone();
    let mut verdicts = Vec::new();

    for w in writes {
        let tree = match head.as_deref() {
            Some(c) => match tree_of_commit(git, c).await {
                Ok(t) => t,
                Err(e) => {
                    verdicts.push((w.reply, Err(e)));
                    continue;
                }
            },
            None => None,
        };
        let plan = match plan_one(git, tree.as_deref(), &w.mutation, w.cap).await {
            Ok(p) => p,
            Err(e) => {
                verdicts.push((w.reply, Err(e)));
                continue;
            }
        };
        // A save that changes nothing is not a commit. A file manager
        // saving an untouched buffer must not add history.
        if Some(plan.tree.as_str()) == tree.as_deref() {
            verdicts.push((w.reply, Ok(plan.etag)));
            continue;
        }
        let message = format!("{} (file API)", w.mutation.summary());
        let parents: Vec<String> = head.iter().cloned().collect();
        match git.commit_tree(&plan.tree, &parents, &message, &w.author).await {
            Ok(commit) => {
                head = Some(commit);
                verdicts.push((w.reply, Ok(plan.etag)));
            }
            Err(e) => verdicts.push((w.reply, Err(FileError::from(e)))),
        }
    }

    let command = match (&tip, &head) {
        (a, b) if a == b => None,
        (_, None) => None,
        (_, Some(new_oid)) => Some(super::gitcmd::RefUpdate {
            name: branch.to_string(),
            old_oid: tip.clone().unwrap_or_else(|| super::gitcmd::zero_oid(40)),
            new_oid: new_oid.clone(),
        }),
    };
    WriteBatch { command, verdicts }
}

/// Answer every caller in a planned batch, turning the batch's own
/// verdict on the ref movement into each caller's answer.
pub fn answer(batch: WriteBatch, moved: Option<Result<(), String>>) {
    for (reply, verdict) in batch.verdicts {
        let answer = match (&verdict, &moved) {
            // Refused during planning: their own answer stands.
            (Err(_), _) => verdict,
            // Nothing needed moving, or the move landed.
            (Ok(_), None) | (Ok(_), Some(Ok(()))) => verdict,
            // The ref movement was refused: everyone in the chain is
            // told the same thing, because none of their commits landed.
            (Ok(_), Some(Err(reason))) => Err(classify(reason)),
        };
        // A caller that hung up is not an error; the write still landed.
        let _ = reply.send(answer);
    }
}
