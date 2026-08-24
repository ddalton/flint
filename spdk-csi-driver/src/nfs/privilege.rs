//! Does this server actually have the privilege it needs? — audit
//! finding 7.
//!
//! # Why this is a startup probe and not a comment
//!
//! An NFS server serving many uids has to stamp the caller's AUTH_SYS
//! identity on the objects it creates: `ioops.rs`'s OPEN(create) and
//! `fileops.rs`'s CREATE both chown the new object to `ctx.unix_cred`.
//! Without that, every file lands owned by the server process and
//! ownership-sensitive workloads refuse to run — postgres checks
//! `st_uid == geteuid()`, and a mode-0700 directory locks out the very
//! client that created it.
//!
//! Both call sites are **best effort**: a chown failure must not fail
//! the OPEN, because the object already exists and unwinding it is
//! worse. That is the right call, and it has a consequence — the server
//! answers NFS4_OK either way. Measured on Linux, with the server as a
//! non-root uid and the capability withheld:
//!
//! ```text
//! with    CAP_CHOWN   file owner 503:1000   (the caller)     ✓
//! without CAP_CHOWN   file owner 65532:988  (the server)     ✗
//! ```
//!
//! …and, before the warnings added alongside this module, **zero log
//! lines** distinguished the two. A whole export can be quietly owned
//! by the wrong uid with every operation reporting success.
//!
//! So the privilege is checked once, at startup, by USING it rather than
//! by reading `/proc/self/status`. A capability bit that is set but
//! non-functional (an fs that ignores chown, a restrictive LSM, a
//! user namespace where the target uid is unmapped) reads as present
//! and behaves as absent; only the syscall knows.
//!
//! # What it does NOT do
//!
//! It does not refuse to start. A hub that will not boot is a worse
//! outcome than one serving with wrong ownership, and some deployments
//! genuinely have a single-uid export where none of this matters. The
//! probe's job is to make the choice visible and attributable, at the
//! moment it becomes true, in the log an operator already reads.

use std::path::Path;
use tracing::{info, warn};

/// The uid the probe tries to chown to. `daemon` (1) exists on every
/// Linux base image this ships on and is never the server's own uid —
/// chowning a file to the uid you already are succeeds without
/// CAP_CHOWN and would make this probe pass while proving nothing.
const PROBE_UID: u32 = 1;

/// Outcome of the startup privilege probe.
#[derive(Debug, PartialEq, Eq)]
pub enum ChownProbe {
    /// The server can stamp caller identity on created objects.
    Capable,
    /// It cannot. Files will be owned by the server, silently.
    Denied(String),
    /// The probe itself could not run — no conclusion either way.
    Inconclusive(String),
}

/// Try one real chown in `export_root` and report what happened.
///
/// The probe file is created and removed inside this call. It is named
/// distinctively so that a crash between create and unlink leaves
/// something an operator can recognise rather than a mystery dotfile.
pub fn probe_chown(export_root: &Path) -> ChownProbe {
    #[cfg(not(unix))]
    {
        let _ = export_root;
        return ChownProbe::Inconclusive("not a unix target".into());
    }
    #[cfg(unix)]
    {
        let probe = export_root.join(".flint-chown-probe");
        let _ = std::fs::remove_file(&probe);
        if let Err(e) = std::fs::write(&probe, b"") {
            return ChownProbe::Inconclusive(format!(
                "could not create a probe file in {}: {e}",
                export_root.display()
            ));
        }
        let res = std::os::unix::fs::chown(&probe, Some(PROBE_UID), None);
        let _ = std::fs::remove_file(&probe);
        match res {
            Ok(()) => ChownProbe::Capable,
            Err(e) => ChownProbe::Denied(e.to_string()),
        }
    }
}

/// Run the probe and say so in the log. Called by every front-end at
/// startup; never fails the boot.
pub fn report_at_startup(export_root: &Path) {
    match probe_chown(export_root) {
        ChownProbe::Capable => {
            info!(
                "privilege: CAP_CHOWN present — created objects will carry the caller's \
                 AUTH_SYS identity"
            );
        }
        ChownProbe::Denied(e) => {
            warn!(
                "⚠ privilege: this server CANNOT chown ({e}). Every file and directory it \
                 creates will be owned by the SERVER, not by the client that created it. \
                 Ownership-sensitive workloads (postgres checks st_uid == geteuid) will \
                 refuse to run, and a mode-0700 directory will lock out its own creator. \
                 Operations still return NFS4_OK — this is the only warning you get. \
                 Grant CAP_CHOWN: as a non-root uid that needs file capabilities on the \
                 binary, because Kubernetes has no ambient capabilities."
            );
        }
        ChownProbe::Inconclusive(e) => {
            warn!("privilege: chown capability could not be determined ({e})");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The probe must report on the ACTUAL syscall. Running as an
    /// ordinary user in CI, chowning to another uid is denied, so the
    /// expected answer here is `Denied` — and if it ever comes back
    /// `Capable` the suite is running as root, which is also a valid
    /// answer. What must never happen is `Inconclusive` on a writable
    /// directory: that would mean the probe silently declined to look.
    #[test]
    fn the_probe_reaches_a_conclusion_on_a_writable_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let out = probe_chown(tmp.path());
        assert!(
            matches!(out, ChownProbe::Capable | ChownProbe::Denied(_)),
            "probe must reach a conclusion on a writable dir, got {out:?}"
        );
    }

    /// An unwritable path is Inconclusive, never Denied — reporting
    /// "you lack CAP_CHOWN" when the real problem is a missing export
    /// would send an operator after the wrong thing entirely.
    #[test]
    fn an_unusable_export_root_is_inconclusive_not_denied() {
        let out = probe_chown(std::path::Path::new("/definitely/not/a/real/export/root"));
        assert!(
            matches!(out, ChownProbe::Inconclusive(_)),
            "a missing export root must not be reported as a capability failure, got {out:?}"
        );
    }

    /// Anti-drift guard, same shape as lockops' `bring_up` lint and the
    /// namespace-commit lint. Both front-ends must report, because a
    /// diagnostic present on one binary and absent from the other is the
    /// §1.1 failure exactly: it stays invisible until it costs
    /// something, and then nobody can tell which binary was silent.
    #[test]
    fn both_front_ends_report_their_privilege_at_startup() {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mains = [base.join("nfs_main.rs"), base.join("nfs_mds_main.rs")];
        let mut reporting = 0usize;
        let mut missing = Vec::new();
        for m in &mains {
            let text = std::fs::read_to_string(m)
                .unwrap_or_else(|e| panic!("main unreadable {}: {e}", m.display()));
            if text.contains("privilege::report_at_startup") {
                reporting += 1;
            } else {
                missing.push(m.display().to_string());
            }
        }
        // Anti-vacuity: a wrong path list would make the loop above
        // examine nothing and the assertion below pass by not looking.
        assert_eq!(
            reporting + missing.len(),
            mains.len(),
            "the lint did not examine every front-end — its paths are stale"
        );
        assert!(
            missing.is_empty(),
            "these NFS front-ends never report whether they can chown, so an export \
             silently owned by the server uid has no first symptom:\n{}",
            missing.join("\n")
        );
    }

    /// The probe must not leave its file behind.
    #[test]
    fn the_probe_cleans_up_after_itself() {
        let tmp = tempfile::tempdir().unwrap();
        let _ = probe_chown(tmp.path());
        assert!(
            !tmp.path().join(".flint-chown-probe").exists(),
            "the probe file must be removed whether or not the chown succeeded"
        );
    }
}
