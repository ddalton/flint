//! Block-layout nvme session management on the CSI node — design doc
//! §5, the csi-node half of "csi-node session management".
//!
//! The kernel blocklayout driver (v6.11+) never connects transports: it
//! resolves the device GETDEVICEINFO names by looking up
//! `/dev/disk/by-id/nvme-eui.<nguid>` and silently degrades every I/O
//! to MDS proxying when the lookup fails. So the session is OUR job,
//! end to end: `nvme connect` with the MDS-admitted host NQN at
//! NodeStage, the fast_io_fail backfill (the D-state landmine class —
//! see `ReconnectPolicy::fast_io_fail_sysfs`), and the §4a by-id link
//! udev does not create (rig-confirmed on systemd 255: SPDK always
//! exposes a UUID descriptor, the kernel wwid prefers it, and udev
//! links only `nvme-uuid.*`). NodeUnstage tears all of it down —
//! including the link, whose dangling remnant the rig found masking the
//! §4a landmine on the NEXT attach.
//!
//! Under subsystem-per-volume (the phase-1 exposure shape) a session is
//! 1 NQN = 1 volume = 1 namespace: no refcounting, no multi-namespace
//! ambiguity. The namespace scan below still tail-picks defensively —
//! across a tgt restart the dying namespace's sysfs dir can coexist
//! briefly with its replacement (rig-observed).
//!
//! Everything here is level-triggered and idempotent: `ensure_session`
//! on an established session repairs the link and the sysfs knobs and
//! touches nothing else; `teardown_session` on a torn-down volume is a
//! no-op. Command failures are loud errors — a session that silently
//! failed to establish is exactly the §4a degradation this module
//! exists to prevent.

use crate::pnfs_csi::BlockAttach;

/// Where the by-id links live. A constant (not configurable) because it
/// is the KERNEL's lookup path — `fs/nfs/blocklayout` hardcodes it.
const BY_ID_DIR: &str = "/dev/disk/by-id";

/// The §4a udev rule, embedded so the binary is the single source of
/// truth (the chart mounts the host rules dir; this code writes it).
/// Proven live on the rig VM (kernel 7.0/systemd 255): `udevadm test`
/// computes the exact `nvme-eui.<bare-hex>` link, a trigger creates
/// it, device removal cleans it (udev-owned — no dangling links), and
/// a FRESH connect re-creates it with no flint involvement — the case
/// the stage-time managed link cannot cover (device re-adds while
/// staged).
const UDEV_RULE: &str = include_str!("../files/99-flint-pnfs-eui.rules");
const UDEV_RULE_PATH: &str = "/etc/udev/rules.d/99-flint-pnfs-eui.rules";

/// Install (or refresh) the §4a udev rule on the host. Called at node
/// startup when the block layout is enabled; the chart mounts the
/// host's `/etc/udev/rules.d` into the node container. udevd watches
/// its rules dirs via inotify and reloads on its own, and existing
/// staged sessions already carry the managed link — so no udevadm is
/// needed, and future add/change events (fresh connects, reconnect
/// renumbering) get the link natively. Returns whether a write
/// happened; an unwritable rules dir is a loud error the caller
/// surfaces without failing startup (the managed link still covers
/// every staged volume).
pub fn install_udev_rule() -> Result<bool, String> {
    let path = std::path::Path::new(UDEV_RULE_PATH);
    match std::fs::read_to_string(path) {
        Ok(current) if current == UDEV_RULE => return Ok(false),
        _ => {}
    }
    std::fs::write(path, UDEV_RULE)
        .map_err(|e| format!("writing {}: {e} (is /etc/udev/rules.d mounted?)", UDEV_RULE_PATH))?;
    Ok(true)
}

/// Establish (or repair) the volume's nvme-tcp session and return the
/// resolved namespace device path (`/dev/nvmeXnY`).
pub async fn ensure_session(attach: &BlockAttach) -> Result<String, String> {
    let policy = crate::nvme_recovery::ReconnectPolicy::from_env();

    // Connect unless a controller for this subsystem already exists.
    // An existing controller is REUSED whatever its state: `connecting`
    // / `resetting` recover on their own (that is what the reconnect
    // policy is for), and a second connect to the same subsystem would
    // either dup the session or fail — neither helps.
    if controller_for_nqn(&attach.subnqn).is_none() {
        let mut args: Vec<String> = vec![
            "connect".into(),
            "-t".into(), "tcp".into(),
            "-a".into(), attach.traddr.clone(),
            "-s".into(), attach.trsvcid.to_string(),
            "-n".into(), attach.subnqn.clone(),
            // The MDS-admitted identity, verbatim — connecting as
            // anything else is refused by the default-closed allow-list.
            "-q".into(), attach.host_nqn.clone(),
        ];
        args.extend(policy.connect_args());
        let out = tokio::process::Command::new("nvme")
            .args(&args)
            .output()
            .await
            .map_err(|e| format!("nvme connect exec: {e}"))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            if !stderr.contains("already connected") {
                return Err(format!(
                    "nvme connect to {} at {}:{} failed: {}",
                    attach.subnqn,
                    attach.traddr,
                    attach.trsvcid,
                    stderr.trim()
                ));
            }
        }
    }

    // Bound queued I/O (best-effort, loud — same contract as the
    // loopback path's backfill: a silently-unbounded controller made
    // the runay wedge invisible).
    apply_fast_io_fail(&attach.subnqn, &policy);

    // Let udev finish before we judge what it created (§4a: checking
    // early and then linking races udev's own database update).
    let _ = tokio::process::Command::new("udevadm")
        .args(["settle", "-t", "10"])
        .output()
        .await;

    // The namespace device node appears asynchronously after connect.
    let dev =
        wait_for_namespace(&attach.nguid, std::time::Duration::from_secs(10)).await?;

    // §4a: ensure /dev/disk/by-id/nvme-eui.<nguid> resolves to the
    // device. udev creates only nvme-uuid.* against SPDK's namespaces
    // (rig-confirmed), and a stale link from a previous incarnation may
    // point at a dead name — both repaired here.
    ensure_eui_link(&attach.nguid, &dev)?;

    Ok(dev)
}

/// Tear the volume's session down: by-id link first (nothing may
/// resolve to a device about to vanish), then `nvme disconnect`.
/// Idempotent; collects both arms' failures rather than stopping at
/// the first, because a half-torn session must still make maximal
/// progress (the rig's dangling-link finding is the cost of stopping
/// early).
pub async fn teardown_session(subnqn: &str, nguid: &str) -> Result<String, String> {
    let mut errors: Vec<String> = Vec::new();
    let link = eui_link_path(nguid);
    let link_removed = match std::fs::remove_file(&link) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => {
            errors.push(format!("removing {}: {e}", link.display()));
            false
        }
    };

    let mut disconnected = false;
    if controller_for_nqn(subnqn).is_some() {
        match tokio::process::Command::new("nvme")
            .args(["disconnect", "-n", subnqn])
            .output()
            .await
        {
            Ok(out) if out.status.success() => disconnected = true,
            Ok(out) => errors.push(format!(
                "nvme disconnect {}: {}",
                subnqn,
                String::from_utf8_lossy(&out.stderr).trim()
            )),
            Err(e) => errors.push(format!("nvme disconnect exec: {e}")),
        }
    }

    if errors.is_empty() {
        Ok(format!(
            "session torn down (disconnected={disconnected} link_removed={link_removed})"
        ))
    } else {
        Err(errors.join("; "))
    }
}

/// The kernel controller (e.g. `nvme2`) whose subsystem NQN is `nqn`,
/// via sysfs. `None` when no session exists — the teardown's "nothing
/// to do" answer and the ensure's "must connect" answer.
pub fn controller_for_nqn(nqn: &str) -> Option<String> {
    let entries = std::fs::read_dir("/sys/class/nvme").ok()?;
    for entry in entries.flatten() {
        if let Ok(subsys) = std::fs::read_to_string(entry.path().join("subsysnqn")) {
            if subsys.trim() == nqn {
                return Some(entry.file_name().to_string_lossy().to_string());
            }
        }
    }
    None
}

/// Write `fast_io_fail_tmo` to every controller serving `nqn`
/// (mirrors the loopback path's backfill; nvme-cli has no connect flag
/// for it).
fn apply_fast_io_fail(nqn: &str, policy: &crate::nvme_recovery::ReconnectPolicy) {
    let Some(value) = policy.fast_io_fail_sysfs() else { return };
    let Ok(entries) = std::fs::read_dir("/sys/class/nvme") else { return };
    for entry in entries.flatten() {
        let Ok(subsys) = std::fs::read_to_string(entry.path().join("subsysnqn")) else {
            continue;
        };
        if subsys.trim() != nqn {
            continue;
        }
        let attr = entry.path().join("fast_io_fail_tmo");
        // Progress goes to STDERR: the CLI wrapper's stdout is a JSON
        // contract (the rig parses it), and the node agent reads both
        // streams into the same pod log anyway.
        match std::fs::write(&attr, &value) {
            Ok(()) => eprintln!(
                "🧯 [pNFS-BLOCK] {} fast_io_fail_tmo={}s",
                entry.file_name().to_string_lossy(),
                value
            ),
            Err(e) => eprintln!(
                "⚠️  [pNFS-BLOCK] could not set fast_io_fail_tmo on {}: {e} — this \
                 controller will QUEUE I/O for the full ctrl_loss_tmo if its target dies",
                entry.file_name().to_string_lossy()
            ),
        }
    }
}

/// Wait for the namespace block device carrying `nguid` to exist and
/// return its `/dev/…` path. Poll-based: the node appears via udev
/// after the kernel's namespace scan, with no event we can await here.
///
/// Resolution is BY NGUID over `/sys/class/block`, never by walking
/// the controller's sysfs dir: under native NVMe multipath the head
/// gendisk (`nvmeXnY`, the only one `bdev_file_open_by_path` — and the
/// kernel blocklayout open — accepts) hangs off the SUBSYSTEM, and the
/// controller dir holds only hidden `nvmeXcYnZ` path devices
/// (rig-proven; a controller-dir scan finds nothing to use there).
/// Matching the NGUID also IS the §4a contract — the same 16 bytes the
/// by-id link must name.
async fn wait_for_namespace(
    nguid: &str,
    timeout: std::time::Duration,
) -> Result<String, String> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(name) = resolve_namespace_by_nguid(nguid) {
            let dev = format!("/dev/{name}");
            if std::path::Path::new(&dev).exists() {
                return Ok(dev);
            }
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "no namespace head device carries NGUID {} after {:?} — the target \
                 may have no namespace for this volume, or udev is wedged",
                nguid, timeout
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

/// The `/sys/class/block` name of the namespace head device whose
/// `nguid` attribute matches (dash- and case-insensitively — the sysfs
/// attr prints dashed, `stable_ns_identity` bare).
fn resolve_namespace_by_nguid(nguid: &str) -> Option<String> {
    let want = normalize_hex(nguid);
    let entries = std::fs::read_dir("/sys/class/block").ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("nvme") || is_multipath_path_device(&name) {
            continue;
        }
        if let Ok(g) = std::fs::read_to_string(entry.path().join("nguid")) {
            if normalize_hex(g.trim()) == want {
                return Some(name);
            }
        }
    }
    None
}

/// Is `name` a hidden per-controller path device (`nvmeXcYnZ`)? Those
/// exist only under native multipath, next to the head device we want,
/// and opening one fails even though it "exists".
fn is_multipath_path_device(name: &str) -> bool {
    let rest = match name.strip_prefix("nvme") {
        Some(r) => r,
        None => return false,
    };
    let mut chars = rest.chars().peekable();
    let mut saw_ctrl_digit = false;
    while matches!(chars.peek(), Some(c) if c.is_ascii_digit()) {
        chars.next();
        saw_ctrl_digit = true;
    }
    saw_ctrl_digit && chars.next() == Some('c')
}

fn normalize_hex(s: &str) -> String {
    s.chars()
        .filter(|c| *c != '-')
        .flat_map(|c| c.to_lowercase())
        .collect()
}

fn eui_link_path(nguid: &str) -> std::path::PathBuf {
    std::path::Path::new(BY_ID_DIR).join(format!("nvme-eui.{nguid}"))
}

/// Make `/dev/disk/by-id/nvme-eui.<nguid>` resolve to `dev`, replacing
/// a wrong or dangling link (previous incarnation, renumbered device).
fn ensure_eui_link(nguid: &str, dev: &str) -> Result<(), String> {
    let link = eui_link_path(nguid);
    match std::fs::read_link(&link) {
        Ok(target) => {
            // Relative targets are resolved against the link's dir,
            // matching how the kernel's open would resolve them.
            let resolved = if target.is_absolute() {
                target
            } else {
                std::path::Path::new(BY_ID_DIR).join(target)
            };
            if resolved == std::path::Path::new(dev) {
                return Ok(());
            }
            std::fs::remove_file(&link)
                .map_err(|e| format!("replacing stale link {}: {e}", link.display()))?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        // Exists but is not a symlink (or unreadable): replace it — the
        // kernel's lookup would fail on it just the same.
        Err(_) => {
            std::fs::remove_file(&link)
                .map_err(|e| format!("replacing non-link {}: {e}", link.display()))?;
        }
    }
    if let Some(parent) = link.parent() {
        // by-id does not exist until udev has processed SOME disk;
        // creating it is what udev itself would do.
        let _ = std::fs::create_dir_all(parent);
    }
    std::os::unix::fs::symlink(dev, &link)
        .map_err(|e| format!("creating {} -> {}: {e}", link.display(), dev))?;
    eprintln!("🔗 [pNFS-BLOCK] {} -> {} (§4a link ensured)", link.display(), dev);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multipath_path_devices_are_excluded_head_devices_kept() {
        // The head gendisk — the only openable one — is kept…
        assert!(!is_multipath_path_device("nvme0n1"));
        assert!(!is_multipath_path_device("nvme12n3"));
        // …the hidden per-controller path devices are not.
        assert!(is_multipath_path_device("nvme0c0n1"));
        assert!(is_multipath_path_device("nvme12c3n1"));
        // Non-nvme names never classify as path devices.
        assert!(!is_multipath_path_device("sda"));
        assert!(!is_multipath_path_device("nvmec0n1"), "no controller digits, no verdict");
    }

    #[test]
    fn the_embedded_udev_rule_has_its_load_bearing_pieces() {
        // Not a udev parser — pins the pieces whose loss would be
        // silent: the eui prefix the kernel looks up, the dash strip
        // (sysfs prints dashed, the kernel wants bare hex), the
        // path-device exclusion, and udev's literal-$ escape ($$ —
        // a single $ would be eaten as a udev substitution).
        assert!(UDEV_RULE.contains("nvme-eui."));
        assert!(UDEV_RULE.contains("tr -d -"));
        assert!(UDEV_RULE.contains("KERNEL!=\"nvme*c*n*\""));
        assert!(UDEV_RULE.contains("$$(echo %s{nguid}"));
        assert!(UDEV_RULE.contains("SYMLINK+=\"disk/by-id/%c\""));
        // Exactly one rule line (comments aside) — a second line is a
        // merge accident.
        assert_eq!(
            UDEV_RULE.lines().filter(|l| l.starts_with("ACTION==")).count(),
            1
        );
    }

    #[test]
    fn nguid_matching_is_dash_and_case_insensitive() {
        // The sysfs attr prints dashed/uuid-shaped; stable_ns_identity
        // is bare lowercase hex. Both normalize to the same key.
        assert_eq!(
            normalize_hex("AABBCCDD-1122-3344-5566-778899aabbcc"),
            "aabbccdd112233445566778899aabbcc"
        );
        assert_eq!(
            normalize_hex("aabbccdd112233445566778899aabbcc"),
            "aabbccdd112233445566778899aabbcc"
        );
    }

    #[test]
    fn eui_link_is_created_replaced_and_kept() {
        // The link functions take absolute paths; exercise them against
        // a scratch dir by calling the pieces directly. BY_ID_DIR is
        // the kernel's hardcoded path, so this test drives
        // `ensure_eui_link`'s logic through a copy of its steps on a
        // temp dir instead of mutating /dev.
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("nvme-eui.abc");
        let dev_a = dir.path().join("nvme0n1");
        let dev_b = dir.path().join("nvme1n1");
        std::fs::write(&dev_a, b"").unwrap();
        std::fs::write(&dev_b, b"").unwrap();

        // Create.
        std::os::unix::fs::symlink(&dev_a, &link).unwrap();
        assert_eq!(std::fs::read_link(&link).unwrap(), dev_a);
        // Replace (device renumbered): remove + relink, what
        // ensure_eui_link does when the target differs.
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&dev_b, &link).unwrap();
        assert_eq!(std::fs::read_link(&link).unwrap(), dev_b);
        // A DANGLING link still read_links fine — the repair path keys
        // on the target comparison, not on existence.
        std::fs::remove_file(&dev_b).unwrap();
        assert_eq!(std::fs::read_link(&link).unwrap(), dev_b);
    }
}
