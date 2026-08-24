//! Durability for namespace operations — audit blocker 2.
//!
//! # The gap this closes
//!
//! Before this module, `sync_all`/`sync_data` appeared in `src/nfs/`
//! at exactly four places: WRITE, COMMIT, COPY and the fence
//! heartbeat. All four are *file content*. **No namespace operation
//! was ever fsynced.** CREATE, REMOVE, RENAME and LINK returned
//! NFS4_OK the instant the syscall returned, with the new dirent
//! living only in the page cache.
//!
//! Linux's own NFS server does not do this. `nfsd` calls
//! `commit_metadata()` after every create, unlink, rename and link,
//! which is an `fsync` on the *parent directory*, and it propagates
//! the failure to the client. The reason is that NFSv4 gives a client
//! **no way to find out**: there is a write verifier for UNSTABLE
//! writes (so a client can discover its data did not survive and
//! resend), but there is no equivalent for the namespace. A client
//! that received NFS4_OK for a RENAME has no protocol mechanism to
//! learn that the rename evaporated in a power cut. It will never ask
//! again.
//!
//! So the pre-fix behaviour was: on a hard power loss, ACKed
//! CREATE/RENAME/REMOVE roll back within the filesystem's journal
//! commit window (ext4's default `commit=5` — up to five seconds of
//! ACKed namespace history), silently, with no client able to detect
//! it and nothing in any log.
//!
//! # What this costs
//!
//! An fsync per namespace op, which is the dominant cost of a
//! metadata-heavy workload — `git clone`, `npm install`, anything that
//! creates many small files. That cost is the *point*: it is what
//! durability is. It is also what the reference implementation
//! charges, so a flint-vs-knfsd comparison is now like-for-like where
//! before flint was winning by not doing the work.
//!
//! [`FLINT_NFS_METADATA_FSYNC=0`] turns it off for deployments that
//! would rather have the throughput and can accept losing ACKed
//! namespace operations on power loss, and for A/B measurement of
//! exactly what it costs. **The default is on**: a durability
//! guarantee that is off by default is not a guarantee.
//!
//! [`FLINT_NFS_METADATA_FSYNC=0`]: self#configuration
//!
//! # Why the parent directory, and why failure is an error
//!
//! The dirent lives in the parent, so the parent is what must be
//! stable. Fsyncing the *file* does not help: the file can be fully
//! durable while the name that reaches it is not.
//!
//! An fsync failure is reported as NFS4ERR_IO rather than logged and
//! swallowed. Swallowing it would reintroduce the exact defect —
//! an ACK for something that is not durable — while looking like it
//! had been fixed, which is worse than not fixing it. `nfsd` does the
//! same (`commit_metadata`'s error becomes the reply's status).

use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};
use tracing::warn;

/// Environment knob: `FLINT_NFS_METADATA_FSYNC`.
///
/// Unset or anything other than `0`/`false`/`off` → **enabled**.
const ENV: &str = "FLINT_NFS_METADATA_FSYNC";

/// 0 = not yet resolved, 1 = enabled, 2 = disabled.
static STATE: AtomicU8 = AtomicU8::new(0);

/// Is namespace-durability enabled? Resolved once from the
/// environment and cached — this is on the per-op path.
pub fn enabled() -> bool {
    match STATE.load(Ordering::Relaxed) {
        1 => true,
        2 => false,
        _ => {
            let on = match std::env::var(ENV) {
                Ok(v) => {
                    let v = v.trim().to_ascii_lowercase();
                    !(v == "0" || v == "false" || v == "off" || v == "no")
                }
                Err(_) => true,
            };
            STATE.store(if on { 1 } else { 2 }, Ordering::Relaxed);
            if !on {
                warn!(
                    "{ENV} is off: CREATE/REMOVE/RENAME/LINK will be ACKed before their \
                     dirent is durable. On power loss the client has NO protocol way to \
                     learn the operation was undone."
                );
            }
            on
        }
    }
}

/// Test-only: force the resolved state, so a test does not depend on
/// the ambient environment (and cannot be perturbed by a sibling test
/// setting the variable).
#[cfg(test)]
pub(crate) fn set_for_test(on: bool) {
    STATE.store(if on { 1 } else { 2 }, Ordering::Relaxed);
}

/// fsync one directory. `Ok(())` when durability is off — the caller's
/// control flow must not change with the knob, only the guarantee.
///
/// Runs the blocking fsync on the blocking pool: on a slow or
/// congested device this is exactly the syscall that parks longest,
/// and parking it on a runtime worker would stall every other
/// connection that worker is driving.
pub async fn commit_dir(dir: &Path) -> std::io::Result<()> {
    if !enabled() {
        return Ok(());
    }
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || {
        // Directories open read-only; fsync(2) on a directory fd is
        // what makes the dirent stable. On Linux this is precisely
        // what nfsd's commit_metadata does.
        let f = std::fs::File::open(&dir)?;
        f.sync_all()
    })
    .await
    .map_err(|e| std::io::Error::other(format!("metadata fsync task failed: {e}")))?
}

/// fsync the parent of `path`. A path with no parent (the export root
/// itself) is a no-op rather than an error.
pub async fn commit_parent_of(path: &Path) -> std::io::Result<()> {
    match path.parent() {
        Some(p) => commit_dir(p).await,
        None => Ok(()),
    }
}

/// fsync two directories, skipping the second when it is the same as
/// the first. RENAME mutates two parents; a same-directory rename
/// mutates one, and fsyncing it twice would double the cost of the
/// most common case for no gain.
pub async fn commit_dirs(a: &Path, b: &Path) -> std::io::Result<()> {
    commit_dir(a).await?;
    if a != b {
        commit_dir(b).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `STATE` is process-global, so a sibling test flipping the knob
    /// between this test's `set_for_test` and its assertion would make
    /// the result depend on scheduling. Every test that TOUCHES the
    /// knob takes this first. (The same class of bug the tier suite
    /// paid for with its process-global capture queue.)
    static KNOB: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// The knob's control flow must be identical either way: only the
    /// guarantee changes, never whether the caller sees an error.
    #[tokio::test]
    async fn disabled_is_a_success_no_op_even_for_a_path_that_does_not_exist() {
        let _g = KNOB.lock().unwrap_or_else(|e| e.into_inner());
        set_for_test(false);
        let missing = std::path::Path::new("/definitely/not/a/real/directory/anywhere");
        assert!(
            commit_dir(missing).await.is_ok(),
            "with durability off the call must not fail — a caller that maps this to \
             NFS4ERR_IO would start refusing operations the moment the knob flipped"
        );
        set_for_test(true);
    }

    #[tokio::test]
    async fn enabled_commits_a_real_directory_and_surfaces_a_bad_one() {
        let _g = KNOB.lock().unwrap_or_else(|e| e.into_inner());
        set_for_test(true);
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("a");
        std::fs::write(&f, b"x").unwrap();

        commit_parent_of(&f)
            .await
            .expect("fsync of a real parent directory must succeed");

        // Anti-vacuity: if commit_dir returned Ok unconditionally the
        // assertion above would pass with the whole mechanism removed.
        // A path that cannot be opened MUST surface as an error, which
        // is what makes the NFS4ERR_IO mapping at the call sites real.
        let missing = tmp.path().join("no-such-dir");
        assert!(
            commit_dir(&missing).await.is_err(),
            "an unopenable directory must produce an error — otherwise the call sites' \
             NFS4ERR_IO arm is dead code and nothing is actually being committed"
        );
    }

    /// Anti-drift guard, same shape as lockops' `bring_up` lint.
    ///
    /// Every namespace-mutating syscall must be followed by a commit
    /// before its success is reported. A behavioural test cannot cover
    /// this — reaching `CreateOp::execute` needs a whole dispatcher,
    /// a filehandle manager and an export — so the call sites are
    /// asserted directly.
    ///
    /// The failure this prevents is silent by construction: drop the
    /// commit and every test still passes, every client still gets
    /// NFS4_OK, and the only difference is what survives a power cut,
    /// which nothing in CI can observe.
    #[test]
    fn every_namespace_mutation_commits_before_it_is_acked() {
        // (file, the syscall that mutates the namespace, how far the
        // commit may be from it)
        const GUARDED: &[(&str, &str, usize)] = &[
            ("nfs/v4/operations/fileops.rs", "tokio::fs::rename(&source_path, &dest_path).await", 40),
            ("nfs/v4/operations/fileops.rs", "tokio::fs::hard_link(&file_path, &link_path).await", 40),
            ("nfs/v4/operations/fileops.rs", "match create_result {", 40),
            ("nfs/v4/operations/fileops.rs", "tokio::fs::remove_file(&target_path).await", 40),
            ("nfs/v4/operations/ioops.rs", "Ok(created) => {", 40),
        ];

        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut checked = 0usize;
        let mut violations = Vec::new();

        for (rel, needle, window) in GUARDED {
            let path = base.join(rel);
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("source unreadable {}: {e}", path.display()));
            // Trim the trailing test module. NOT `find("#[cfg(test)]")`
            // — `ioops.rs` carries an INDENTED `#[cfg(test)]` attribute
            // on an item partway up the file, and truncating there hid
            // two thirds of the production code from this lint, which
            // then failed for the wrong reason. Anchor on a cfg(test)
            // at column 0, which is the file-level test module, and take
            // the LAST one.
            let prod = match text.rfind("\n#[cfg(test)]") {
                Some(i) => &text[..i],
                None => &text[..],
            };
            let Some(at) = prod.find(needle) else {
                violations.push(format!(
                    "{rel}: anchor {needle:?} not found — this lint's anchors are stale and \
                     it would pass by not looking"
                ));
                continue;
            };
            checked += 1;
            let after: String = prod[at..].lines().take(*window).collect::<Vec<_>>().join("\n");
            if !after.contains("metadata_sync::commit") {
                violations.push(format!(
                    "{rel}: no `metadata_sync::commit*` within {window} lines of {needle:?} — \
                     this operation is ACKed before its dirent is durable, and NFSv4 gives \
                     the client no way to find out"
                ));
            }
        }

        // Anti-vacuity: if no anchor resolved, the loop above proved
        // nothing and every `contains` would have been skipped.
        assert_eq!(
            checked,
            GUARDED.len(),
            "expected to check {} namespace-mutation sites, checked {checked} — \
             the anchors are stale",
            GUARDED.len()
        );
        assert!(violations.is_empty(), "{}", violations.join("\n"));
    }

    /// A same-directory rename must not pay twice.
    #[tokio::test]
    async fn commit_dirs_handles_the_same_directory_twice() {
        let _g = KNOB.lock().unwrap_or_else(|e| e.into_inner());
        set_for_test(true);
        let tmp = tempfile::tempdir().unwrap();
        commit_dirs(tmp.path(), tmp.path())
            .await
            .expect("same-directory rename must commit cleanly");

        let other = tmp.path().join("sub");
        std::fs::create_dir(&other).unwrap();
        commit_dirs(tmp.path(), &other)
            .await
            .expect("cross-directory rename must commit both parents");
    }
}
