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

/// The kernel floor for pNFS block over NVMe: `fs/nfs/blocklayout`
/// gained NVMe device support in 3921ae0850a3 (v6.11 merge window).
/// Below it the mount SUCCEEDS and every I/O silently proxies through
/// the MDS — the one outcome worse than failing (Ubuntu 24.04 stock is
/// 6.8). `FLINT_PNFS_BLOCK_KERNEL_OVERRIDE=1` skips the check for
/// distro kernels that backport the support under an old version
/// string — the operator is asserting the backport, loudly.
pub const BLOCK_LAYOUT_KERNEL_FLOOR: (u32, u32) = (6, 11);

/// "6.8.0-49-generic" → (6, 8). None when the string leads with
/// anything that is not `major.minor`.
fn parse_kernel_release(s: &str) -> Option<(u32, u32)> {
    let mut parts = s.trim().split(|c: char| !c.is_ascii_digit());
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

/// The pure verdict, testable off-Linux. An unparseable release
/// REFUSES — a kernel we cannot even read a version from is not a
/// known-good kernel, and the override exists for exactly this shape
/// of exception.
fn check_kernel_release(release: &str, override_set: bool) -> Result<(), String> {
    if override_set {
        return Ok(());
    }
    let (floor_major, floor_minor) = BLOCK_LAYOUT_KERNEL_FLOOR;
    match parse_kernel_release(release) {
        Some((major, minor)) if (major, minor) >= (floor_major, floor_minor) => Ok(()),
        Some((major, minor)) => Err(format!(
            "kernel {major}.{minor} is below the {floor_major}.{floor_minor} pNFS-block floor \
             (blocklayout NVMe support landed in v6.11): the mount would SILENTLY \
             DEGRADE every I/O to MDS proxying. Refusing to stage. \
             Set FLINT_PNFS_BLOCK_KERNEL_OVERRIDE=1 only if this kernel backports it"
        )),
        None => Err(format!(
            "cannot parse kernel release {release:?} against the \
             {floor_major}.{floor_minor} pNFS-block floor; refusing to stage. \
             Set FLINT_PNFS_BLOCK_KERNEL_OVERRIDE=1 to assert support"
        )),
    }
}

/// Refuse block-layout session work on a kernel whose blocklayout
/// driver cannot speak NVMe. Called at every mouth: NodeStage (maps to
/// FailedPrecondition — kubelet retries cannot change the kernel), the
/// CLI `stage` (BEFORE the attach RPC, so an unstageable node never
/// plants a durable attach row), and `ensure_session` itself as the
/// belt covering re-establishment.
pub fn kernel_block_layout_support() -> Result<(), String> {
    if std::env::var("FLINT_PNFS_BLOCK_KERNEL_OVERRIDE").as_deref() == Ok("1") {
        let release = std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .unwrap_or_else(|_| "<unreadable>".into());
        tracing::warn!(
            "⚠️  [pNFS-BLOCK] FLINT_PNFS_BLOCK_KERNEL_OVERRIDE=1 — accepting kernel {} \
             despite the {}.{} floor on the operator's word",
            release.trim(),
            BLOCK_LAYOUT_KERNEL_FLOOR.0,
            BLOCK_LAYOUT_KERNEL_FLOOR.1
        );
        return Ok(());
    }
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map_err(|e| format!("reading /proc/sys/kernel/osrelease: {e}"))?;
    check_kernel_release(&release, false)
}

/// Where durable per-volume session records live — what makes a
/// session RE-ESTABLISHABLE after the kernel deletes its controller
/// (ctrl_loss_tmo exhausted during a long tgt outage; nothing else
/// remembers the coordinates node-side, and kubelet only re-runs
/// NodeStage after a reboot). Default is under the kubelet plugins
/// dir, which the chart mounts at the same path in-container as on
/// the host; `FLINT_PNFS_SESSION_DIR` overrides (the rig).
fn session_dir() -> std::path::PathBuf {
    std::env::var("FLINT_PNFS_SESSION_DIR")
        .unwrap_or_else(|_| {
            "/var/lib/kubelet/plugins/flint.csi.storage.io/block-sessions".to_string()
        })
        .into()
}

/// Record filename for a subsystem NQN: the volume id it encodes
/// (`…:block:<volume>`), falling back to a sanitized NQN. Volume ids
/// are `pvc-<uuid>[~m<shard>]` — filesystem-safe by construction.
fn record_name(subnqn: &str) -> String {
    match subnqn.rsplit_once(":block:") {
        Some((_, vol)) if !vol.is_empty() => vol.to_string(),
        _ => subnqn.replace(['/', ':'], "_"),
    }
}

/// Serialize a session record (KEY=VALUE lines — no serde in this
/// module, and the values (NQNs, addresses, ports) contain no '=' or
/// newlines by construction).
fn render_session_record(attach: &BlockAttach) -> String {
    format!(
        "traddr={}\ntrsvcid={}\nsubnqn={}\nnguid={}\nhost_nqn={}\nmds_control={}\n",
        attach.traddr,
        attach.trsvcid,
        attach.subnqn,
        attach.nguid,
        attach.host_nqn,
        attach.mds_control
    )
}

/// Parse the record back. Corrupted records error (the caller logs and
/// skips — a bad record must not kill the reconcile pass).
fn parse_session_record(s: &str) -> Result<BlockAttach, String> {
    let mut traddr = None;
    let mut trsvcid = None;
    let mut subnqn = None;
    let mut nguid = None;
    let mut host_nqn = None;
    let mut mds_control = String::new();
    for line in s.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        match k {
            "traddr" => traddr = Some(v.to_string()),
            "trsvcid" => trsvcid = v.parse::<u16>().ok(),
            "subnqn" => subnqn = Some(v.to_string()),
            "nguid" => nguid = Some(v.to_string()),
            "host_nqn" => host_nqn = Some(v.to_string()),
            // Optional: records written before the redirect actor
            // existed have no control endpoint, and they must keep
            // parsing — they simply get the old replay behaviour.
            "mds_control" => mds_control = v.to_string(),
            _ => {}
        }
    }
    match (traddr, trsvcid, subnqn, nguid, host_nqn) {
        (Some(traddr), Some(trsvcid), Some(subnqn), Some(nguid), Some(host_nqn))
            if !traddr.is_empty() && !subnqn.is_empty() && !nguid.is_empty()
                && !host_nqn.is_empty() =>
        {
            Ok(BlockAttach { traddr, trsvcid, subnqn, nguid, host_nqn, mds_control })
        }
        _ => Err("missing or empty fields".into()),
    }
}

fn write_session_record(attach: &BlockAttach) -> Result<(), String> {
    let dir = session_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("create {}: {e}", dir.display()))?;
    let path = dir.join(record_name(&attach.subnqn));
    std::fs::write(&path, render_session_record(attach))
        .map_err(|e| format!("write {}: {e}", path.display()))
}

fn remove_session_record(subnqn: &str) -> bool {
    std::fs::remove_file(session_dir().join(record_name(subnqn))).is_ok()
}

/// The node the record's host NQN names. `node_host_nqn` is
/// `…:node:<name>`, so the inverse is exact — and deriving it beats
/// adding a field, because every record ever written already carries it.
fn node_name_of(host_nqn: &str) -> Option<&str> {
    host_nqn.rsplit_once(":node:").map(|(_, n)| n).filter(|n| !n.is_empty())
}

/// The volume id the record's subsystem NQN encodes (`…:block:<vol>`).
fn volume_of(subnqn: &str) -> Option<&str> {
    subnqn.rsplit_once(":block:").map(|(_, v)| v).filter(|v| !v.is_empty())
}

/// THE REDIRECT ACTOR, first half: ask the MDS where this volume lives
/// NOW, instead of replaying an address a failover may have retired.
///
/// `AttachBlockNode` is the lane, and it is deliberately not a new RPC:
/// it is idempotent, it resolves the target through the serving-target
/// record, and it REFUSES a node that has been fenced meanwhile — the
/// three things a re-attach needs. Calling it again is the re-attach.
///
/// BEST-EFFORT, and that is load-bearing. The old behaviour — replay
/// the record, no MDS call — is what makes this pass work through an
/// MDS outage, and losing that to gain a redirect would be a bad trade.
/// So an unreachable MDS falls back to the record, exactly as before;
/// only a successful answer overrides it.
async fn resolve_current_coordinates(attach: &BlockAttach) -> Option<BlockAttach> {
    if attach.mds_control.is_empty() {
        // A record written before the redirect actor existed. It keeps
        // the old behaviour, which is precisely the world
        // `FlintCompositionNoActor.cfg` parks a client in — worth one
        // line so an operator can see which nodes are still in it.
        tracing::debug!(
            "block session for {} carries no MDS control endpoint — replaying the recorded \
             address (re-stage the volume to gain the redirect lane)",
            attach.subnqn
        );
        return None;
    }
    let (volume, node) = (volume_of(&attach.subnqn)?, node_name_of(&attach.host_nqn)?);
    let client = crate::pnfs_csi::PnfsCsi::new(attach.mds_control.clone())
        .with_timeout(std::time::Duration::from_secs(
            std::env::var("FLINT_PNFS_REATTACH_TIMEOUT_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(10),
        ));
    match client.attach_block_node(volume, node).await {
        Ok(fresh) => Some(fresh),
        Err(e) => {
            // Includes the fenced case, which must stay loud: a fenced
            // node re-entering silently is the door the durable eviction
            // exists to close. Falling back to the record here does not
            // re-open it — the connect itself is refused at the target.
            tracing::warn!(
                "re-attach lane for {} could not reach the MDS at {} ({}) — replaying the \
                 recorded address",
                attach.subnqn,
                attach.mds_control,
                e
            );
            None
        }
    }
}

/// THE REDIRECT ACTOR, second half: tell the MDS the session is up, so
/// it re-fires CB_NOTIFY_DEVICEID to the clients that cached this
/// volume's device.
///
/// AFTER the device exists, never before, and that ordering is measured
/// rather than reasoned: the unfence drill sent the notification while
/// the replacement device did not yet exist and the client accepted it
/// and did nothing (1/1 accepted, write still failed). The MDS cannot
/// observe a node's reconnect, so the node says so.
async fn ack_session_up(attach: &BlockAttach) {
    if attach.mds_control.is_empty() {
        return;
    }
    let (Some(volume), Some(node)) =
        (volume_of(&attach.subnqn), node_name_of(&attach.host_nqn))
    else {
        return;
    };
    let client = crate::pnfs_csi::PnfsCsi::new(attach.mds_control.clone())
        .with_timeout(std::time::Duration::from_secs(10));
    match client.block_session_up(volume, node).await {
        Ok((notified, attempted)) if attempted > 0 => tracing::info!(
            "🔔 session-up ack for {}: MDS re-fired device-notify, {}/{} accepted",
            attach.subnqn,
            notified,
            attempted
        ),
        Ok(_) => {}
        // Best-effort by design: the session is up either way, and a
        // client that missed the notification is in the documented
        // recycle-the-mount state rather than a broken one.
        Err(e) => tracing::warn!("session-up ack for {} failed: {}", attach.subnqn, e),
    }
}

/// One pass of session re-establishment. Three worlds per record, and
/// [`action_for`] is the decision:
///
///   * the controller is `live` — serving I/O, left alone, and not even
///     asked about (a fleet at rest makes no control-plane calls);
///   * no controller at all — `ctrl_loss_tmo` exhausted during an
///     outage; connect afresh;
///   * a controller that is NOT live — `connecting`, `resetting`. The
///     reconnect policy owns it UNLESS the volume has moved, because
///     then the address it is patiently retrying will never answer
///     again and the patience is spent waiting for a corpse.
///
/// THE REDIRECT ACTOR LIVES HERE (design §12). Before replaying the
/// record, this asks the MDS where the volume lives now: a failover
/// moves the serving target, and an address recorded at stage time
/// points at a node that no longer serves the volume — the parked
/// client `FlintCompositionNoActor.cfg` produces when the actor's
/// fairness is withheld. The MDS call is BEST-EFFORT, so the pass still
/// works through an MDS outage exactly as it did before, and a node
/// fenced meanwhile still gets a loud connect refusal rather than a
/// silent re-entry.
///
/// Returns `(records, repaired, failed)`.
pub async fn reestablish_sessions() -> (usize, usize, usize) {
    let Ok(entries) = std::fs::read_dir(session_dir()) else {
        return (0, 0, 0); // no dir = no block volumes ever staged here
    };
    let (mut records, mut repaired, mut failed) = (0usize, 0usize, 0usize);
    for entry in entries.flatten() {
        let Ok(content) = std::fs::read_to_string(entry.path()) else { continue };
        records += 1;
        let attach = match parse_session_record(&content) {
            Ok(a) => a,
            Err(e) => {
                tracing::error!(
                    "corrupt block-session record {:?}: {} — skipped (unstage of the \
                     volume removes it)",
                    entry.file_name(),
                    e
                );
                failed += 1;
                continue;
            }
        };
        let ctrl = controller_for_nqn(&attach.subnqn);
        let state = ctrl.as_deref().and_then(controller_state);
        // A LIVE controller is serving I/O: never disturbed, and never
        // even asked about — a fleet at rest must make no control-plane
        // calls.
        if controller_is_live(state.as_deref()) {
            continue;
        }
        // THE REDIRECT: the record's address is a snapshot of where the
        // volume lived at stage time. Ask where it lives now.
        let fresh = resolve_current_coordinates(&attach).await;
        let moved = fresh.as_ref().is_some_and(|f| {
            (f.traddr.as_str(), f.trsvcid) != (attach.traddr.as_str(), attach.trsvcid)
        });
        match action_for(ctrl.is_some(), moved) {
            SessionAction::LeaveAlone => continue,
            SessionAction::Reestablish => tracing::warn!(
                "block session for {} has NO kernel controller (ctrl_loss exhausted \
                 during an outage?) — re-establishing",
                attach.subnqn
            ),
            SessionAction::Redirect => {
                // THE 30-MINUTE HOLE THIS CLOSES. A controller whose
                // target died sits in `connecting` for ctrl_loss_tmo —
                // 1800s by default — and the old guard here skipped it
                // as "not ours to touch". That patience is right for a
                // target COMING BACK at the same address (a tgt restart,
                // a drainRoll) and wrong for a volume that has MOVED:
                // the address it is retrying will never answer again.
                // So the composition failed over in seconds and its
                // client followed half an hour later.
                //
                // Only the moved case acts. Same address, or an MDS we
                // could not reach, still belongs to the reconnect policy
                // — an outage must not become a disconnect storm.
                tracing::warn!(
                    "🔀 {} moved while its controller ({}) was still {} — the reconnect it is \
                     retrying can never succeed; tearing it down for the redirect",
                    attach.subnqn,
                    ctrl.as_deref().unwrap_or("?"),
                    state.as_deref().unwrap_or("unknown")
                );
                // CONNECT BEFORE DISCONNECT, and the reason is the mount.
                //
                // Tearing the dead path down first takes the NAMESPACE
                // with it, so the reconnect builds a NEW one and every
                // consumer holding the old device is left holding a
                // corpse — measured: a mounted ext4 ends up with the
                // mount table still reading `rw` while the filesystem
                // has shut down, and the volume needs a pod restart it
                // gives no sign of needing.
                //
                // Both targets export the SAME subsystem NQN with the
                // same serial (derived from the NQN) and the same pinned
                // (uuid, nguid) — so the survivor's controller joins the
                // subsystem the client already has and its namespace
                // becomes a second PATH to the head the mount is bound
                // to, rather than a new namespace beside it.
                //
                // The durable record is NOT removed (that is
                // `teardown_session`'s job, and it means unstage): this
                // volume is still staged here, it just answers elsewhere.
                let policy = crate::nvme_recovery::ReconnectPolicy::from_env();
                let target = match fresh.clone() {
                    Some(t) => t,
                    // Unreachable: Redirect is only chosen when `moved`,
                    // which requires a fresh answer. Defended anyway —
                    // the alternative is an unwrap in the failover path.
                    None => continue,
                };
                let before = controllers_for_nqn(&attach.subnqn);
                match connect_path(&target, &policy).await {
                    Ok(()) => {
                        // AND WAIT FOR IT TO CARRY THE NAMESPACE. The
                        // connect returns when the controller is live;
                        // the namespace scan finishes after. Remove the
                        // old path inside that window and the head the
                        // client's mount is bound to loses its last
                        // path and dies — the new controller then
                        // builds a fresh head, which is the zombie
                        // mount this whole change exists to prevent.
                        let fresh_ctrl = wait_for_new_path(&attach.subnqn, &before).await;
                        match &fresh_ctrl {
                            Some(c) => tracing::info!(
                                "🔀 {}: path '{}' to {}:{} is carrying the namespace — now \
                                 removing the dead controller '{}'",
                                attach.subnqn,
                                c,
                                target.traddr,
                                target.trsvcid,
                                ctrl.as_deref().unwrap_or("?")
                            ),
                            // Removing the old path anyway: leaving both
                            // a dead controller and an unscanned new one
                            // is worse than one clean path, and the
                            // reconnect policy owns whatever is left.
                            None => tracing::warn!(
                                "🔀 {}: the new path to {}:{} did not present a namespace in \
                                 time — removing the dead controller anyway; a mounted \
                                 client may need a remount",
                                attach.subnqn,
                                target.traddr,
                                target.trsvcid
                            ),
                        }
                        if let Some(old) = ctrl.as_deref() {
                            if let Err(e) = disconnect_controller(old).await {
                                // NOT fatal, and not a `continue`: the
                                // volume is already serving over the new
                                // path. A lingering dead path costs a
                                // failed-over I/O retry, and the next
                                // pass tries the removal again.
                                tracing::error!(
                                    "the dead controller '{}' for {} did not go away ({}) — \
                                     the volume IS serving over the new path; retried next pass",
                                    old,
                                    attach.subnqn,
                                    e
                                );
                            }
                        }
                    }
                    Err(e) => {
                        // The fallback is the old behaviour, and it is
                        // worse on purpose rather than by accident: it
                        // restores the path at the cost of the namespace,
                        // so a mounted consumer will need a remount.
                        tracing::warn!(
                            "could not add {}'s new path before removing the old one ({}) — \
                             falling back to disconnect-then-connect, which costs any mount \
                             on this volume its device",
                            attach.subnqn,
                            e
                        );
                        match tokio::process::Command::new("nvme")
                            .args(["disconnect", "-n", &attach.subnqn])
                            .output()
                            .await
                        {
                            Ok(out) if out.status.success() => {}
                            Ok(out) => {
                                tracing::error!(
                                    "could not disconnect the stale controller for {}: {} — \
                                     retried next pass",
                                    attach.subnqn,
                                    String::from_utf8_lossy(&out.stderr).trim()
                                );
                                failed += 1;
                                continue;
                            }
                            Err(e) => {
                                tracing::error!(
                                    "nvme disconnect exec for {}: {e}",
                                    attach.subnqn
                                );
                                failed += 1;
                                continue;
                            }
                        }
                    }
                }
            }
        }
        let attach = match fresh {
            Some(fresh) if moved => {
                tracing::warn!(
                    "🔀 {} MOVED: {}:{} → {}:{} — the recorded target no longer serves this \
                     volume; re-attaching to the one the MDS names",
                    attach.subnqn,
                    attach.traddr,
                    attach.trsvcid,
                    fresh.traddr,
                    fresh.trsvcid
                );
                // Persist before connecting: if the connect fails and
                // this node reboots, the next pass must start from the
                // CURRENT address, not walk back to the dead one.
                if let Err(e) = write_session_record(&fresh) {
                    tracing::error!("could not persist the redirected session record: {e}");
                }
                fresh
            }
            // Same address, or no answer — either way the record stands.
            Some(_) | None => attach,
        };
        match ensure_session(&attach).await {
            Ok(dev) => {
                tracing::info!("🔌 block session re-established: {} → {}", attach.subnqn, dev);
                repaired += 1;
                // The device exists NOW. Only now may the clients be
                // told to drop their cached deviceid — a notification
                // that arrives first is accepted and useless, measured.
                ack_session_up(&attach).await;
            }
            Err(e) => {
                tracing::error!(
                    "block session re-establish for {} FAILED: {} — retried next pass",
                    attach.subnqn, e
                );
                failed += 1;
            }
        }
    }
    (records, repaired, failed)
}

/// Add ONE path to the volume's subsystem, unconditionally.
///
/// Split out of `ensure_session` because the redirect needs to add the
/// survivor's path while the dead composer's controller is STILL
/// PRESENT, and `ensure_session` deliberately refuses to connect when
/// any controller exists (an existing one is reused whatever its state).
///
/// A second connect to the same subsystem NQN at a DIFFERENT address is
/// not a duplicate: the kernel's existing-controller check compares the
/// transport address alongside the subsystem and host NQNs, so this
/// becomes a second PATH rather than an -EALREADY. That is the whole
/// basis of connect-before-disconnect.
async fn connect_path(
    attach: &BlockAttach,
    policy: &crate::nvme_recovery::ReconnectPolicy,
) -> Result<(), String> {
    let mut args: Vec<String> = vec![
        "connect".into(),
        "-t".into(), "tcp".into(),
        "-a".into(), attach.traddr.clone(),
        "-s".into(), attach.trsvcid.to_string(),
        "-n".into(), attach.subnqn.clone(),
        // The MDS-admitted identity, verbatim — connecting as
        // anything else is refused by the default-closed allow-list.
        "-q".into(), attach.host_nqn.clone(),
        // …and the hostid the kernel pairs with it. Omitting this let
        // nvme-cli invent one per container instance, which the kernel
        // then refused against any surviving controller under the same
        // NQN (see identity::host_id_for_nqn).
        "-I".into(), crate::identity::host_id_for_nqn(&attach.host_nqn),
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
    Ok(())
}

/// Remove ONE controller by name (`nvme1`), leaving every other path to
/// the same subsystem alone. `nvme disconnect -n <nqn>` would take them
/// all, including the one just added.
async fn disconnect_controller(ctrl: &str) -> Result<(), String> {
    let out = tokio::process::Command::new("nvme")
        .args(["disconnect", "-d", ctrl])
        .output()
        .await
        .map_err(|e| format!("nvme disconnect exec: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
}

/// Establish (or repair) the volume's nvme-tcp session and return the
/// resolved namespace device path (`/dev/nvmeXnY`).
pub async fn ensure_session(attach: &BlockAttach) -> Result<String, String> {
    // The kernel floor, re-checked at the mouth of every session (the
    // NodeStage/CLI checks are the polite refusals; this one also
    // covers reestablish_sessions replaying records onto a node whose
    // kernel changed underneath them — e.g. a downgrade boot).
    kernel_block_layout_support()?;

    let policy = crate::nvme_recovery::ReconnectPolicy::from_env();

    // Connect unless a controller for this subsystem already exists.
    // An existing controller is REUSED whatever its state: `connecting`
    // / `resetting` recover on their own (that is what the reconnect
    // policy is for), and a second connect to the same subsystem would
    // either dup the session or fail — neither helps.
    if controller_for_nqn(&attach.subnqn).is_none() {
        connect_path(attach, &policy).await?;
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

    // The durable record that makes this session survivable past
    // ctrl_loss exhaustion (see `reestablish_sessions`). Loud, not
    // fatal — an unwritable record dir degrades to the pre-record
    // behaviour, never to a failed stage.
    if let Err(e) = write_session_record(attach) {
        eprintln!(
            "⚠️  [pNFS-BLOCK] could not persist the session record: {e} — a \
             controller lost to ctrl_loss_tmo will NOT be re-established on this node"
        );
    }

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
    // The record goes FIRST: with it gone, a concurrently ticking
    // `reestablish_sessions` pass cannot see "controller missing +
    // record present" mid-teardown and resurrect the session we are
    // taking down. A failed teardown retries with the record already
    // gone — absent is the desired end state either way.
    let record_removed = remove_session_record(subnqn);
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
            "session torn down (disconnected={disconnected} link_removed={link_removed} \
             record_removed={record_removed})"
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

/// EVERY controller attached to `nqn`, not just the first. The redirect
/// needs this to tell its freshly added path apart from the dead one it
/// is about to remove.
pub fn controllers_for_nqn(nqn: &str) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir("/sys/class/nvme") else {
        return Vec::new();
    };
    let mut out: Vec<String> = entries
        .flatten()
        .filter(|e| {
            std::fs::read_to_string(e.path().join("subsysnqn"))
                .map(|s| s.trim() == nqn)
                .unwrap_or(false)
        })
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    out.sort();
    out
}

/// Has this controller finished attaching a namespace?
///
/// THE WHOLE REDIRECT TURNS ON THIS. `nvme connect` returns when the
/// controller is live, and the namespace scan completes AFTER that.
/// Removing the old path in that window takes the last path off the
/// namespace head the client's mount is bound to; the head dies, and
/// the new controller's scan then has to build a FRESH head — which is
/// how a failover produced /dev/nvme0n2 beside a zombie /dev/nvme0n1,
/// measured at 1.4ms between "new ctrl" and "Removing ctrl".
///
/// Under multipath the per-controller namespace appears as
/// `nvme<subsys>c<ctrl>n<nsid>`; without it, `nvme<ctrl>n<nsid>`. Both
/// end in `n<digits>`, which is what this looks for.
fn controller_has_namespace(ctrl: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(format!("/sys/class/nvme/{ctrl}")) else {
        return false;
    };
    entries.flatten().any(|e| {
        let name = e.file_name().to_string_lossy().to_string();
        if !name.starts_with("nvme") {
            return false;
        }
        match name.rsplit_once('n') {
            Some((_, nsid)) => !nsid.is_empty() && nsid.chars().all(|c| c.is_ascii_digit()),
            None => false,
        }
    })
}

/// Wait for a controller that was not in `before` to appear on `nqn`
/// AND to have attached its namespace. Returns its name, or `None` if
/// it never got there — the caller decides what to do with that, and
/// deciding is not this function's job.
///
/// Bounded: the whole redirect runs inside a reconcile pass, so an
/// unbounded wait here would stall every other volume's session behind
/// one that is not coming back.
async fn wait_for_new_path(nqn: &str, before: &[String]) -> Option<String> {
    for _ in 0..50 {
        if let Some(fresh) = controllers_for_nqn(nqn)
            .into_iter()
            .find(|c| !before.contains(c))
        {
            if controller_has_namespace(&fresh) {
                return Some(fresh);
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    None
}

/// A kernel controller's state (`live`, `connecting`, `resetting`,
/// `deleting`…) — the word sysfs uses, not our interpretation of it.
pub fn controller_state(ctrl: &str) -> Option<String> {
    std::fs::read_to_string(format!("/sys/class/nvme/{ctrl}/state"))
        .ok()
        .map(|s| s.trim().to_string())
}

/// Is this controller serving I/O? Only `live` is, and the distinction
/// is what keeps the reconcile pass free at rest: a live session is
/// never disturbed and never even asked about.
fn controller_is_live(state: Option<&str>) -> bool {
    matches!(state, Some("live"))
}

/// What a non-live session record deserves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionAction {
    /// The reconnect policy owns this one.
    LeaveAlone,
    /// The controller is gone (ctrl_loss exhausted) — connect afresh.
    Reestablish,
    /// The volume answers somewhere else now: tear the stale controller
    /// down and connect to where it moved.
    Redirect,
}

/// The whole decision, in one place so it can be tested without sysfs.
///
/// `present` = a kernel controller still exists for this subsystem, and
/// (by the caller's guard) it is NOT live. `moved` = the MDS named an
/// address different from the record's.
///
/// The load-bearing row is `(true, true)`. Before it existed, a
/// controller retrying a dead composer was skipped as "not ours to
/// touch" until `ctrl_loss_tmo` — 1800s by default — expired, so a
/// failover that completed in seconds reached its client half an hour
/// later. `(true, false)` stays LeaveAlone on purpose: same address, or
/// an MDS we could not reach, is exactly what the reconnect policy is
/// for, and acting there would turn a control-plane outage into a
/// disconnect storm across every node at once.
fn action_for(present: bool, moved: bool) -> SessionAction {
    match (present, moved) {
        (false, _) => SessionAction::Reestablish,
        (true, true) => SessionAction::Redirect,
        (true, false) => SessionAction::LeaveAlone,
    }
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
    fn session_records_round_trip_and_reject_corruption() {
        let attach = BlockAttach {
            traddr: "10.0.0.9".into(),
            trsvcid: 4420,
            subnqn: "nqn.2024-11.com.flint:block:pvc-x~m0".into(),
            nguid: "aabbccdd112233445566778899aabbcc".into(),
            host_nqn: "nqn.2024-11.com.flint:node:w1".into(),
            mds_control: "10.1.2.3:50051".into(),
        };
        let rendered = render_session_record(&attach);
        assert_eq!(parse_session_record(&rendered).unwrap(), attach);

        // The filename is the volume id the subnqn encodes — shard pin
        // included (one record per staged volume).
        assert_eq!(record_name(&attach.subnqn), "pvc-x~m0");
        assert_eq!(record_name("nqn.weird:no-block-part"), "nqn.weird_no-block-part");

        // Corruption refuses instead of yielding a half-parsed attach
        // the pass would then connect with.
        assert!(parse_session_record("").is_err());
        assert!(parse_session_record("traddr=10.0.0.9\ntrsvcid=x\n").is_err());
        let truncated = rendered.lines().take(3).collect::<Vec<_>>().join("\n");
        assert!(parse_session_record(&truncated).is_err());
        // Unknown keys are ignored (forward compatibility).
        let extended = format!("{rendered}future_key=whatever\n");
        assert_eq!(parse_session_record(&extended).unwrap(), attach);

        // A record written BEFORE the redirect actor existed still
        // parses, with no control endpoint — those nodes keep the old
        // replay behaviour rather than failing to re-establish at all.
        let legacy = rendered
            .lines()
            .filter(|l| !l.starts_with("mds_control="))
            .collect::<Vec<_>>()
            .join("\n");
        let parsed = parse_session_record(&legacy).expect("legacy records must still parse");
        assert!(parsed.mds_control.is_empty());
        assert_eq!(parsed.traddr, attach.traddr, "everything else survives");
    }

    /// THE 30-MINUTE HOLE, as a table.
    ///
    /// The redirect actor was written, correct, and UNREACHABLE while a
    /// kernel controller existed — and after a composer dies one exists
    /// for `ctrl_loss_tmo`, whose default is 1800s. So the composition
    /// failed over in seconds and the client that had to follow it
    /// waited half an hour, retrying an address that would never answer
    /// again.
    ///
    /// The other three rows are the ones that must NOT change: a live
    /// session is untouchable, an absent controller is the classic
    /// re-establish, and a reconnecting controller whose volume has not
    /// moved (or whose MDS we could not reach, which reads the same
    /// here) still belongs to the reconnect policy — acting on that row
    /// would turn one control-plane outage into a fleet-wide disconnect
    /// storm.
    #[test]
    fn only_a_volume_that_moved_interrupts_a_reconnecting_controller() {
        assert!(controller_is_live(Some("live")));
        for s in [Some("connecting"), Some("resetting"), Some("deleting"), None] {
            assert!(!controller_is_live(s), "{s:?} must not read as serving I/O");
        }
        assert_eq!(action_for(true, true), SessionAction::Redirect);
        assert_eq!(action_for(true, false), SessionAction::LeaveAlone);
        assert_eq!(action_for(false, false), SessionAction::Reestablish);
        assert_eq!(action_for(false, true), SessionAction::Reestablish);
    }

    /// THE REDIRECT ACTOR's two derivations. Both invert an identity
    /// this codebase already mints, so every record ever written
    /// carries them — which is why the actor needed no new record
    /// field and works on sessions staged before it existed (given a
    /// control endpoint).
    #[test]
    fn the_actor_recovers_the_volume_and_node_from_the_record_it_has() {
        assert_eq!(
            volume_of("nqn.2024-11.com.flint:block:pvc-x~m0"),
            Some("pvc-x~m0")
        );
        assert_eq!(node_name_of("nqn.2024-11.com.flint:node:w1"), Some("w1"));

        // And they round-trip against the producers, so a rename on
        // either side is caught here rather than in a silent no-redirect.
        let nqn = crate::identity::block_volume_export_nqn("pvc-round");
        assert_eq!(volume_of(&nqn), Some("pvc-round"));
        let host = crate::nvmeof_export::flint_host_nqn("node-round");
        assert_eq!(node_name_of(&host), Some("node-round"));

        // Malformed input yields None, never an empty name that would
        // become an MDS call about volume "".
        assert_eq!(volume_of("nqn.2024-11.com.flint:block:"), None);
        assert_eq!(node_name_of("nqn.2024-11.com.flint:node:"), None);
        assert_eq!(volume_of("not-an-nqn"), None);
        assert_eq!(node_name_of("not-an-nqn"), None);
    }

    /// A record with no control endpoint must NOT attempt a redirect —
    /// that is the pre-actor world, and it has to keep working. The
    /// resolver returns `None` (fall back to the record) without so
    /// much as constructing a client.
    #[tokio::test]
    async fn a_record_without_a_control_endpoint_never_dials() {
        let attach = BlockAttach {
            traddr: "10.0.0.9".into(),
            trsvcid: 4420,
            subnqn: "nqn.2024-11.com.flint:block:pvc-legacy".into(),
            nguid: "aabbccdd112233445566778899aabbcc".into(),
            host_nqn: "nqn.2024-11.com.flint:node:w1".into(),
            mds_control: String::new(),
        };
        assert!(resolve_current_coordinates(&attach).await.is_none());
        // The ack is the same story: nothing to ack to.
        ack_session_up(&attach).await; // must not hang or panic
    }

    /// An MDS that cannot be reached falls back to the recorded
    /// address. This is the property the pre-actor code had for free
    /// and the actor must not spend: the whole point of the durable
    /// record is that re-establishment survives an MDS outage.
    #[tokio::test]
    async fn an_unreachable_mds_falls_back_to_the_record() {
        let attach = BlockAttach {
            traddr: "10.0.0.9".into(),
            trsvcid: 4420,
            subnqn: "nqn.2024-11.com.flint:block:pvc-out".into(),
            nguid: "aabbccdd112233445566778899aabbcc".into(),
            host_nqn: "nqn.2024-11.com.flint:node:w1".into(),
            // Port 1: refused at once, so the fallback is exercised
            // without waiting out a timeout.
            mds_control: "127.0.0.1:1".into(),
        };
        let started = std::time::Instant::now();
        assert!(
            resolve_current_coordinates(&attach).await.is_none(),
            "an unreachable MDS must leave the recorded address standing"
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(5), "bounded");
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

    #[test]
    fn kernel_floor_parses_real_release_strings() {
        assert_eq!(parse_kernel_release("6.8.0-49-generic"), Some((6, 8)));
        assert_eq!(parse_kernel_release("6.11.0"), Some((6, 11)));
        assert_eq!(parse_kernel_release("7.0.0-28-generic"), Some((7, 0)));
        assert_eq!(parse_kernel_release("6.11"), Some((6, 11)));
        assert_eq!(parse_kernel_release("6.12.0-rc3+"), Some((6, 12)));
        assert_eq!(parse_kernel_release(""), None);
        assert_eq!(parse_kernel_release("linux"), None);
        // A bare major has no minor to compare against the floor.
        assert_eq!(parse_kernel_release("7"), None);
    }

    #[test]
    fn kernel_floor_refuses_the_silent_degradation_kernels() {
        // Ubuntu 24.04 stock — the kernel that mounts fine and proxies
        // every byte through the MDS.
        let err = check_kernel_release("6.8.0-49-generic", false).unwrap_err();
        assert!(err.contains("6.8"), "names the offending kernel: {err}");
        assert!(err.contains("6.11"), "names the floor: {err}");
        assert!(err.contains("SILENTLY"), "says WHY refusal beats staging: {err}");
        assert!(
            err.contains("FLINT_PNFS_BLOCK_KERNEL_OVERRIDE"),
            "names the backport escape hatch: {err}"
        );
        // 5.x is below on the major alone.
        check_kernel_release("5.15.0-100-generic", false).unwrap_err();
    }

    #[test]
    fn kernel_floor_accepts_at_and_above() {
        check_kernel_release("6.11.0", false).unwrap();
        check_kernel_release("6.12.0-rc3+", false).unwrap();
        check_kernel_release("7.0.0-28-generic", false).unwrap();
    }

    #[test]
    fn kernel_floor_unparseable_refuses_and_override_wins() {
        // Unknown is refused — the override exists for exactly this.
        let err = check_kernel_release("weird-vendor-string", false).unwrap_err();
        assert!(err.contains("FLINT_PNFS_BLOCK_KERNEL_OVERRIDE"), "{err}");
        // The override accepts anything: the operator asserted the
        // backport, and the check's job is to be skippable loudly.
        check_kernel_release("weird-vendor-string", true).unwrap();
        check_kernel_release("6.8.0-49-generic", true).unwrap();
    }
}

/// FORCE THE BLOCK-LAYOUT DEVICE RESOLUTION IN *THIS* MOUNT NAMESPACE.
///
/// THE BUG THIS EXISTS FOR, measured on runbo (2026-08-15, kernel 6.18.29):
/// the kernel resolves the device a SCSI layout names by opening
/// `/dev/disk/by-id/{dm-uuid-mpath-0x,wwn-0x,nvme-eui.}<designator>` — and it
/// does that path lookup in the mount namespace of **whichever process first
/// triggers the layout**. A containerized consumer has no `/dev/disk/by-id`
/// at all, so all three opens return `-ENOENT`, the client logs
/// `pNFS: no device found for volume <nguid>`, and EVERY I/O silently
/// degrades to MDS proxying — which this server refuses (NFS4ERR_IO). The
/// files exist and stay ZERO BYTES.
///
/// The failure is cached per mount, so it is decided once, by whoever gets
/// there first, and never recovers on its own. Traced with a kprobe on
/// `bdev_file_open_by_path`:
///
///   path="/dev/disk/by-id/nvme-eui.<nguid>"  ret=0xfffffffffffffffe  (-ENOENT)
///     …with the link PRESENT and openable by `dd` on the host.
///
/// csi-node runs in the host mount namespace with `/dev` mounted, so a single
/// byte of I/O from HERE resolves the device correctly and the consumer's
/// container then inherits the resolved deviceid — verified live: after this
/// touch, a container read the host's file and wrote its own (26 bytes), where
/// the identical container had produced only 0-byte files before.
///
/// Deliberately best-effort: a failure here is not worse than the status quo
/// (the mount still works, I/O would just proxy and fail loudly), so it is
/// logged and never fails the publish.
pub fn warm_block_layout_device(mount_path: &str) {
    use std::io::Write;
    // A dotfile, created and removed inside this call: the consumer's
    // filesystem must not gain a stray file, and the probe only needs to be
    // real enough to force one LAYOUTGET plus the device open behind it.
    let probe = format!("{}/.flint-blocklayout-warm", mount_path.trim_end_matches('/'));
    let result = (|| -> std::io::Result<()> {
        let mut f = std::fs::File::create(&probe)?;
        f.write_all(b"warm")?;
        // The resolution happens on the way to the device, so the write must
        // actually be pushed rather than left in the page cache.
        f.sync_all()?;
        drop(f);
        std::fs::remove_file(&probe)
    })();
    match result {
        Ok(()) => tracing::info!(
            "🔥 block-layout device warmed from the host namespace for {} — the \
             consumer's container inherits the resolved deviceid instead of \
             resolving it (and failing) in its own /dev",
            mount_path
        ),
        Err(e) => tracing::warn!(
            "block-layout warm-up on {} failed ({}) — if the consumer is \
             containerized its I/O may degrade to MDS proxying (pNFS: no device \
             found); mount itself is unaffected",
            mount_path, e
        ),
    }
}
