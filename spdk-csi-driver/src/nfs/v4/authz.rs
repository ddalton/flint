//! POSIX permission checking against the CALLER — audit finding 7,
//! plan §2A legs A2/A3.
//!
//! # What was here before
//!
//! Nothing. `Nfs4Status::Access` was produced at seventeen sites and
//! every one of them was a mapping of a syscall's `EACCES` — the server
//! delegated authorization entirely to the kernel, which evaluates it
//! against the *server's* identity. Running as root, or as a non-root
//! uid holding `CAP_DAC_OVERRIDE` (which it must, to serve many uids),
//! that check cannot fail. And ACCESS itself returned `op.access &
//! 0x3f` — an echo of the question.
//!
//! Measured with pjdfstest against a knfsd control (2026-08-24): **645
//! assertions failed on flint that pass on knfsd**, dominated by
//! `open ... expected EACCES, got 0` and `chown ... expected EPERM, got
//! 0`. Any client could read any file regardless of mode, and chown any
//! file to anyone.
//!
//! # The rollout, and why it is not on by default
//!
//! Turning this on changes which operations succeed on live mounts.
//! Deployments may be relying — unknowingly — on the absence of checks,
//! and a hub that starts refusing writes is an outage. So
//! [`Mode::Warn`] exists: evaluate, log every request that *would* be
//! denied, deny nothing. That makes the blast radius measurable before
//! it is taken.
//!
//! `FLINT_NFS_ENFORCE_PERMISSIONS`:
//!   * unset / `warn` → evaluate and log, allow everything (default)
//!   * `1` / `enforce` → deny
//!   * `0` / `off` → do not evaluate at all
//!
//! # Known limits, stated rather than discovered later
//!
//! * **The uid is not authenticated.** AUTH_SYS carries no verifier
//!   (RFC 5531), so every identity here is a claim. This is defence
//!   against accident and misconfiguration, not against an attacker who
//!   can reach port 2049. Closing that is leg A6.
//! * **uid 0 is honoured as root**, i.e. `no_root_squash`. That matches
//!   both the pre-existing behaviour and the knfsd export the control
//!   arm runs against. Squashing is a separate, deliberate decision.
//! * ACLs are not consulted; this is the mode-bit model only.

use crate::nfs::v4::protocol::Nfs4Status;
use std::sync::atomic::{AtomicU8, Ordering};
use tracing::warn;

/// Permission bits, in the POSIX sense.
pub const R: u32 = 0b100;
pub const W: u32 = 0b010;
pub const X: u32 = 0b001;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Do not evaluate. Indistinguishable from the pre-fix server.
    Off,
    /// Evaluate and log what would be denied; deny nothing.
    Warn,
    /// Evaluate and deny.
    Enforce,
}

const ENV: &str = "FLINT_NFS_ENFORCE_PERMISSIONS";
static STATE: AtomicU8 = AtomicU8::new(0); // 0=unresolved 1=off 2=warn 3=enforce

pub fn mode() -> Mode {
    match STATE.load(Ordering::Relaxed) {
        1 => Mode::Off,
        2 => Mode::Warn,
        3 => Mode::Enforce,
        _ => {
            let m = match std::env::var(ENV).map(|v| v.trim().to_ascii_lowercase()) {
                Ok(v) if v == "1" || v == "enforce" || v == "true" || v == "on" => Mode::Enforce,
                Ok(v) if v == "0" || v == "off" || v == "false" => Mode::Off,
                // Anything else — including an unparseable value — lands
                // on Warn. A typo must not silently disable the
                // evaluation, and must not silently start denying
                // either; Warn is the only value that is safe when the
                // operator's intent is unclear.
                _ => Mode::Warn,
            };
            STATE.store(
                match m {
                    Mode::Off => 1,
                    Mode::Warn => 2,
                    Mode::Enforce => 3,
                },
                Ordering::Relaxed,
            );
            m
        }
    }
}

#[cfg(test)]
pub(crate) fn set_for_test(m: Mode) {
    STATE.store(
        match m {
            Mode::Off => 1,
            Mode::Warn => 2,
            Mode::Enforce => 3,
        },
        Ordering::Relaxed,
    );
}

/// The caller's AUTH_SYS identity.
#[derive(Debug, Clone, Default)]
pub struct Cred {
    pub uid: u32,
    pub gid: u32,
    /// Supplementary groups from `authsys_parms.gids<16>`. Decoding
    /// these matters: without them a user whose access comes from a
    /// supplementary group is wrongly DENIED, which turns a security fix
    /// into an availability bug.
    pub gids: Vec<u32>,
}

impl Cred {
    pub fn is_root(&self) -> bool {
        self.uid == 0
    }
    fn in_group(&self, gid: u32) -> bool {
        self.gid == gid || self.gids.contains(&gid)
    }
}

/// Which of `want` (R/W/X) the mode bits grant this caller.
///
/// The classic three-tier POSIX rule: owner bits if the uid matches,
/// else group bits if any of the caller's groups match, else other.
/// **Not** a union — a file mode `0466` denies its OWNER write access,
/// and getting that wrong is the single most common way to write this
/// function incorrectly.
pub fn permits(cred: &Cred, file_uid: u32, file_gid: u32, mode_bits: u32, want: u32) -> bool {
    if cred.is_root() {
        // Root bypasses rwx, except that execute needs at least one
        // execute bit somewhere — matching the kernel.
        if want & X != 0 && mode_bits & 0o111 == 0 {
            return false;
        }
        return true;
    }
    let granted = if cred.uid == file_uid {
        (mode_bits >> 6) & 0o7
    } else if cred.in_group(file_gid) {
        (mode_bits >> 3) & 0o7
    } else {
        mode_bits & 0o7
    };
    granted & want == want
}

/// Fetch identity+mode for a path.
#[cfg(unix)]
pub fn ident_of(md: &std::fs::Metadata) -> (u32, u32, u32) {
    use std::os::unix::fs::MetadataExt;
    (md.uid(), md.gid(), md.mode() & 0o7777)
}

/// Evaluate, and turn the answer into a status according to [`mode`].
///
/// `Ok(())` means proceed. In [`Mode::Warn`] a denial logs and still
/// returns `Ok(())` — the whole point of that mode.
pub fn check(
    cred: Option<&Cred>,
    md: &std::fs::Metadata,
    want: u32,
    what: &str,
    path: &std::path::Path,
) -> Result<(), Nfs4Status> {
    let m = mode();
    if m == Mode::Off {
        return Ok(());
    }
    // No credential means AUTH_NONE or GSS, or the hub's own in-process
    // file API. There is no uid to evaluate, so there is nothing this
    // function can honestly say; refusing would break the file API and
    // inventing a uid would be worse.
    let Some(cred) = cred else { return Ok(()) };

    #[cfg(unix)]
    {
        let (fuid, fgid, bits) = ident_of(md);
        if permits(cred, fuid, fgid, bits, want) {
            return Ok(());
        }
        match m {
            Mode::Enforce => {
                warn!(
                    "DENIED {what} on {:?} for uid={} gid={} (file {}:{} mode {:o})",
                    path, cred.uid, cred.gid, fuid, fgid, bits
                );
                Err(Nfs4Status::Access)
            }
            _ => {
                warn!(
                    "WOULD DENY {what} on {:?} for uid={} gid={} (file {}:{} mode {:o}) — \
                     {ENV} is not set to enforce, so this is being ALLOWED. Set it to 1 \
                     once these are understood.",
                    path, cred.uid, cred.gid, fuid, fgid, bits
                );
                Ok(())
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (cred, md, want, what, path);
        Ok(())
    }
}

/// Ownership-change rule (SETATTR owner / owner_group), which is NOT the
/// rwx rule: POSIX `chown(2)` is root-only for the uid, and the owner
/// may change the gid only to a group they belong to. Write permission
/// on the file is irrelevant — this is the check whose absence let any
/// client chown any file to anyone (24 pjdfstest assertions).
pub fn check_chown(
    cred: Option<&Cred>,
    md: &std::fs::Metadata,
    want_uid: Option<u32>,
    want_gid: Option<u32>,
    path: &std::path::Path,
) -> Result<(), Nfs4Status> {
    let m = mode();
    if m == Mode::Off {
        return Ok(());
    }
    let Some(cred) = cred else { return Ok(()) };
    #[cfg(unix)]
    {
        let (fuid, fgid, _) = ident_of(md);
        let allowed = if cred.is_root() {
            true
        } else {
            // Changing the uid at all is root-only. A no-op "change" to
            // the value it already holds is permitted, because that is
            // what the kernel does and pjdfstest relies on it.
            let uid_ok = match want_uid {
                None => true,
                Some(u) => u == fuid,
            };
            let gid_ok = match want_gid {
                None => true,
                Some(g) => g == fgid || cred.in_group(g),
            };
            // …and only the owner may change anything at all.
            uid_ok && gid_ok && cred.uid == fuid
        };
        if allowed {
            return Ok(());
        }
        match m {
            Mode::Enforce => {
                warn!(
                    "DENIED chown {:?}->{:?}:{:?} on {:?} for uid={} (file {}:{})",
                    path, want_uid, want_gid, path, cred.uid, fuid, fgid
                );
                // POSIX says EPERM here, not EACCES.
                Err(Nfs4Status::Perm)
            }
            _ => {
                warn!(
                    "WOULD DENY chown on {:?} for uid={} (file owned by {}) — {ENV} is not \
                     set to enforce, so this is being ALLOWED.",
                    path, cred.uid, fuid
                );
                Ok(())
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (cred, md, want_uid, want_gid, path);
        Ok(())
    }
}

/// Mode-change rule (SETATTR mode): owner or root only.
pub fn check_chmod(
    cred: Option<&Cred>,
    md: &std::fs::Metadata,
    path: &std::path::Path,
) -> Result<(), Nfs4Status> {
    let m = mode();
    if m == Mode::Off {
        return Ok(());
    }
    let Some(cred) = cred else { return Ok(()) };
    #[cfg(unix)]
    {
        let (fuid, _, _) = ident_of(md);
        if cred.is_root() || cred.uid == fuid {
            return Ok(());
        }
        match m {
            Mode::Enforce => {
                warn!("DENIED chmod on {:?} for uid={} (file owned by {})", path, cred.uid, fuid);
                Err(Nfs4Status::Perm)
            }
            _ => {
                warn!(
                    "WOULD DENY chmod on {:?} for uid={} (owner {}) — {ENV} not enforcing.",
                    path, cred.uid, fuid
                );
                Ok(())
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (cred, md, path);
        Ok(())
    }
}

/// Sticky-bit rule for removing or renaming a name out of a directory.
/// In a `1777` directory (the /tmp shape) only the owner of the FILE, the
/// owner of the DIRECTORY, or root may unlink it — write permission on
/// the directory is not enough.
#[cfg(unix)]
pub fn sticky_permits(cred: &Cred, dir_md: &std::fs::Metadata, victim_md: &std::fs::Metadata) -> bool {
    let (duid, _, dbits) = ident_of(dir_md);
    if dbits & 0o1000 == 0 {
        return true; // not sticky
    }
    let (vuid, _, _) = ident_of(victim_md);
    cred.is_root() || cred.uid == vuid || cred.uid == duid
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `STATE` is process-global and the lib tests run in parallel, so a
    /// sibling flipping the mode between this test's `set_for_test` and
    /// its assertion makes the result depend on scheduling. Every test
    /// that touches the mode takes this first. (Same class as the tier
    /// suite's process-global capture queue.)
    static MODE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn cred(uid: u32, gid: u32) -> Cred {
        Cred { uid, gid, gids: vec![] }
    }

    #[test]
    fn owner_group_other_are_tiers_not_a_union() {
        // 0466: owner r--, group rw-, other rw-. The OWNER must be
        // denied write even though group and other are granted it —
        // this is the classic way to get the POSIX rule wrong, and a
        // union implementation passes every other test in this file.
        assert!(!permits(&cred(100, 100), 100, 100, 0o466, W),
            "owner must be denied write by 0466 — the tiers are exclusive, not a union");
        assert!(permits(&cred(101, 100), 100, 100, 0o466, W),
            "a group member IS granted write by 0466");
        assert!(permits(&cred(102, 102), 100, 100, 0o466, W),
            "other IS granted write by 0466");
    }

    #[test]
    fn a_0600_file_is_private_to_its_owner() {
        assert!(permits(&cred(100, 100), 100, 100, 0o600, R | W));
        assert!(!permits(&cred(101, 101), 100, 100, 0o600, R));
        assert!(!permits(&cred(101, 100), 100, 100, 0o600, R), "group has no bits in 0600");
    }

    #[test]
    fn supplementary_groups_grant_access() {
        // Without decoding authsys_parms.gids<16> this user is denied,
        // which would turn a security fix into an availability bug.
        let c = Cred { uid: 100, gid: 999, gids: vec![42, 500] };
        assert!(permits(&c, 1, 42, 0o060, R | W),
            "a supplementary group membership must grant group access");
    }

    #[test]
    fn root_bypasses_rwx_but_not_execute_on_a_non_executable() {
        assert!(permits(&cred(0, 0), 100, 100, 0o000, R | W));
        assert!(!permits(&cred(0, 0), 100, 100, 0o644, X),
            "even root may not execute a file with no execute bit set anywhere");
        assert!(permits(&cred(0, 0), 100, 100, 0o755, X));
    }

    #[test]
    fn chown_is_root_only_and_chgrp_needs_membership() {
        let _g = MODE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("f");
        std::fs::write(&f, b"x").unwrap();
        let md = std::fs::metadata(&f).unwrap();
        let (fuid, fgid, _) = ident_of(&md);
        set_for_test(Mode::Enforce);

        // A non-owner cannot chown at all — the 24-assertion failure.
        let other = Cred { uid: fuid.wrapping_add(1), gid: fgid, gids: vec![] };
        assert!(check_chown(Some(&other), &md, Some(other.uid), None, &f).is_err(),
            "a non-owner must not be able to chown a file to themselves");

        // The owner may not give the file away either.
        let owner = Cred { uid: fuid, gid: fgid, gids: vec![] };
        assert!(check_chown(Some(&owner), &md, Some(fuid + 5), None, &f).is_err(),
            "even the owner may not change the uid — that is root-only");

        // The owner may set the gid to a group they belong to.
        let owner_multi = Cred { uid: fuid, gid: fgid, gids: vec![4242] };
        assert!(check_chown(Some(&owner_multi), &md, None, Some(4242), &f).is_ok());
        assert!(check_chown(Some(&owner_multi), &md, None, Some(9999), &f).is_err(),
            "a group the caller does not belong to must be refused");
        set_for_test(Mode::Warn);
    }

    /// Warn mode must evaluate and LOG but never deny — that property is
    /// what makes it safe to ship on by default.
    #[test]
    fn warn_mode_never_denies() {
        let _g = MODE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("f");
        std::fs::write(&f, b"x").unwrap();
        let md = std::fs::metadata(&f).unwrap();
        set_for_test(Mode::Warn);
        let nobody = Cred { uid: 65533, gid: 65533, gids: vec![] };
        assert!(check(Some(&nobody), &md, R | W, "read", &f).is_ok());
        assert!(check_chown(Some(&nobody), &md, Some(1), None, &f).is_ok());
        assert!(check_chmod(Some(&nobody), &md, &f).is_ok());

        // ...and Enforce on the same inputs MUST deny, or the test above
        // is passing because the check is broken rather than because the
        // mode is doing its job.
        set_for_test(Mode::Enforce);
        let denied = check(Some(&nobody), &md, W, "write", &f).is_err()
            || check_chown(Some(&nobody), &md, Some(1), None, &f).is_err();
        assert!(denied, "Enforce must deny what Warn merely logged");
        set_for_test(Mode::Warn);
    }

    #[test]
    fn no_credential_is_allowed_because_there_is_nothing_to_evaluate() {
        let _g = MODE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("f");
        std::fs::write(&f, b"x").unwrap();
        let md = std::fs::metadata(&f).unwrap();
        set_for_test(Mode::Enforce);
        assert!(check(None, &md, R | W, "read", &f).is_ok(),
            "AUTH_NONE / GSS / the hub's own file API have no uid; refusing them would \
             break the file API and inventing one would be worse");
        set_for_test(Mode::Warn);
    }
}
