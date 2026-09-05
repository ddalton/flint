//! Start-up: claim, rotate, restore, prove, serve (design §5).
//!
//! The local repository is a cache and the snapshot is the truth, so
//! start-up reconciles the cache TO the snapshot in both directions —
//! a ref the bucket does not name is deleted locally, not kept. The
//! asymmetry is deliberate: the only way a local ref can be ahead of
//! the snapshot is a bug, and preserving it would let a repository
//! serve history no other server could restore.
//!
//! Two rules from lean, for the same reasons lean has them. A pack the
//! snapshot names and the bucket lacks is re-read ONCE before being
//! believed — a repack under the previous holder can move the list
//! while this reader is fetching. And a repository that cannot be
//! proved (`fsck --connectivity-only`) is refused loudly rather than
//! served: half a repository serves clones that succeed and check out
//! nothing.

use std::collections::{BTreeMap, BTreeSet};

use flint_store::StoreError;

use super::{gitcmd, packio, snapshot, ForgeError, ForgeResult, Syncer};

/// `merge-tree -X ours|theirs` is 2.43; below it the option means
/// something else, so the floor is asserted once at start rather than
/// discovered inside a merge.
pub const GIT_FLOOR: (u32, u32) = (2, 43);

pub async fn check_git_floor(sc: &Syncer) -> ForgeResult<()> {
    let (major, minor) = sc.git.version().await?;
    if (major, minor) < GIT_FLOOR {
        return Err(ForgeError::Refused(format!(
            "git {major}.{minor} is below forge's floor {}.{} (merge-tree -X)",
            GIT_FLOOR.0, GIT_FLOOR.1
        )));
    }
    Ok(())
}

/// Bring the local repository to exactly what the snapshot names.
pub async fn restore(sc: &mut Syncer) -> ForgeResult<()> {
    sc.check_fence()?;
    let branch = sc.cfg.default_branch.clone();
    let hooks = sc.cfg.hooks_path.clone();
    sc.git.init_bare(&branch, hooks.as_deref()).await?;

    let mut cell = match sc.cell.clone() {
        // A takeover rotation already loaded (and rewrote) it.
        Some(c) => c,
        None => snapshot::load(sc.store.as_ref(), &sc.cfg).await?,
    };
    if cell.etag.is_none() {
        // A repository nobody has published: an empty bare repo is
        // exactly right, and the first batch creates the snapshot under
        // `If-None-Match: *`.
        sc.cell = Some(cell);
        return Ok(());
    }

    let mut listed = list_pack_files(sc).await?;
    let mut revalidated = false;
    loop {
        let missing: Vec<String> = cell
            .snap
            .packs
            .iter()
            .filter(|p| !listed.contains_key(*p))
            .cloned()
            .collect();
        if missing.is_empty() {
            break;
        }
        if revalidated {
            return Err(ForgeError::Refused(format!(
                "snapshot {} names {} pack(s) the bucket does not hold ({}) — refusing to serve a \
                 repository that cannot be restored",
                sc.cfg.snapshot_key(),
                missing.len(),
                missing.join(", ")
            )));
        }
        // A repack under the previous holder can move the pack list
        // while this reader is fetching. Re-read once before believing
        // the absence (lean's revalidate rule).
        revalidated = true;
        cell = snapshot::load(sc.store.as_ref(), &sc.cfg).await?;
        listed = list_pack_files(sc).await?;
    }

    // Fetch every file that belongs to a named pack: the pack itself,
    // its index, and the bitmap and reverse index when they exist. The
    // bitmap is what makes the restored repository clone-ready without
    // a local `repack -b` (§8). All of it goes through one fan-out
    // bounded by `fanout`, across files and chunks alike: one file at a
    // time paid a round trip per sibling in series, and one chunk at a
    // time made a repacked repository's single pack a single stream.
    let pack_dir = sc.cfg.repo.join("objects/pack");
    std::fs::create_dir_all(&pack_dir)?;
    let mut units = Vec::new();
    for pack in &cell.snap.packs {
        let stem = pack.trim_end_matches(".pack");
        for (name, obj) in listed.iter() {
            if name.starts_with(stem) {
                let dest = pack_dir.join(name);
                if dest.exists() {
                    continue;
                }
                units.push(packio::FetchUnit {
                    key: obj.key.clone(),
                    dest,
                    size: obj.size,
                    etag: obj.etag.clone(),
                });
            }
        }
    }
    packio::fetch_all(sc.store.clone(), units, sc.cfg.fanout).await?;

    // Refs: the snapshot's set, exactly. `update-ref --stdin` verifies
    // each object exists, so this is also the first proof that the
    // packs we just fetched contain what the refs name.
    let local = sc.git.refs().await?;
    let mut script = String::new();
    let want: BTreeMap<&String, &String> = cell.snap.refs.iter().collect();
    for (name, oid) in &want {
        if local.get(*name).map(|l| l == *oid).unwrap_or(false) {
            continue;
        }
        script.push_str(&format!("update {name} {oid}\n"));
    }
    let keep: BTreeSet<&String> = cell.snap.refs.keys().collect();
    for (name, oid) in &local {
        if !keep.contains(name) {
            script.push_str(&format!("delete {name} {oid}\n"));
        }
    }
    if !script.is_empty() {
        let out = sc.git.run(&["update-ref", "--stdin"], Some(script.as_bytes())).await?;
        if !out.ok() {
            return Err(ForgeError::Refused(format!(
                "restore could not install the snapshot's refs: {}",
                out.stderr.trim()
            )));
        }
    }

    // HEAD, from the derived object if the bucket has one.
    match sc.store.get_whole(&sc.cfg.head_key(), None).await {
        Ok((_, body)) => {
            let text = String::from_utf8_lossy(&body);
            if let Some(target) = text.trim().strip_prefix("ref: ") {
                if target.starts_with("refs/") {
                    sc.git.symbolic_head(target).await?;
                }
            }
        }
        Err(StoreError::NotFound(_)) => {}
        Err(e) => return Err(e.into()),
    }

    sc.git.fsck_connectivity().await?;
    sc.cell = Some(cell);
    Ok(())
}

/// Every object under the repository's pack prefix, by file name. One
/// LIST serves both the restore's fetch plan and the sweep's candidate
/// set.
pub async fn list_pack_files(sc: &Syncer) -> ForgeResult<BTreeMap<String, PackObject>> {
    let prefix = sc.cfg.pack_prefix();
    let mut out = BTreeMap::new();
    for obj in sc.store.list(&prefix).await? {
        if let Some(name) = obj.key.rsplit('/').next() {
            out.insert(
                name.to_string(),
                PackObject { key: obj.key.clone(), size: obj.size, etag: obj.etag.clone() },
            );
        }
    }
    Ok(out)
}

/// What `list` already told us about a pack file. The size and etag
/// were previously discarded and then not available to the fetch, which
/// is why it read whole objects; carrying them costs nothing (they ride
/// the same LIST) and is what lets the restore fetch ranges pinned to
/// one generation without a HEAD per file.
#[derive(Debug, Clone)]
pub struct PackObject {
    pub key: String,
    pub size: u64,
    pub etag: String,
}

/// Repack when the pack count passes the threshold, then publish the
/// consolidated pack and drop what it supersedes.
///
/// The syncer owns this because git's own auto-gc would be a second,
/// unowned writer of `objects/pack/` — able to delete a pack mid-upload
/// and to produce a consolidated pack that must reach the bucket before
/// the next push can be acknowledged (design §10).
pub async fn maybe_repack(sc: &mut Syncer) -> ForgeResult<bool> {
    sc.check_fence()?;
    let before = sc.git.local_packs()?;
    if before.len() <= sc.cfg.repack_threshold {
        return Ok(false);
    }
    sc.git.repack().await?;
    let after = sc.git.local_packs()?;
    let cell = sc.cell()?.clone();
    let epoch = sc.lease()?.epoch;
    let known: BTreeSet<&String> = cell.snap.packs.iter().collect();
    for pack in &after {
        if known.contains(pack) {
            continue;
        }
        for file in sc.git.pack_siblings(pack) {
            let path = sc.git.pack_path(&file);
            packio::upload_file(sc.store.as_ref(), &sc.cfg.pack_key(&file), &path, epoch).await?;
        }
    }
    let mut next = cell.snap.clone();
    next.packs = after;
    let writer = sc.holder_id.clone();
    let new_cell =
        match snapshot::cas(sc.store.as_ref(), &sc.cfg, &cell, next, epoch, &writer).await {
            Ok(c) => c,
            Err(ForgeError::Store(StoreError::PreconditionFailed(e))) => {
                return Err(sc.fence(format!(
                    "snapshot CAS refused during repack, another server holds this repository: {e}"
                )))
            }
            Err(e) => return Err(e),
        };
    sc.cell = Some(new_cell);
    Ok(true)
}

/// A repository with no refs and no snapshot: set the default branch so
/// a first clone is not a detached mystery.
pub async fn set_default_branch(sc: &Syncer, branch: &str) -> ForgeResult<()> {
    let target = if branch.starts_with("refs/") {
        branch.to_string()
    } else {
        format!("refs/heads/{branch}")
    };
    if sc.git.refs().await?.is_empty() {
        sc.git.symbolic_head(&target).await?;
    }
    Ok(())
}

/// Re-exported for the serving loop's convenience.
pub use gitcmd::RefUpdate;
