//! UBLK_U_CMD_DEL_DEV escape hatch for DEAD ublk devices.
//!
//! The incident (runy2, 2026-07-21): an spdk-tgt SIGKILLed under a wedged
//! containerd leaves its kernel ublk device DEAD rather than quiesced —
//! `ublk_recover_disk` answers ENODEV (no daemon to re-attach) and
//! `ublk_start_disk` on the same id ALSO answers ENODEV (the corpse still
//! occupies the id). Nothing in SPDK's RPC surface can reclaim the id, so
//! the only escape was a node reboot. The kernel-native escape is
//! UBLK_U_CMD_DEL_DEV issued as an io_uring `uring_cmd` on
//! `/dev/ublk-control` — exactly what `ublk del -n <id>` does — which
//! frees the id for a fresh ADD_DEV on the next detector tick.
//!
//! Scope guard: the caller must only invoke this after the DEAD-device
//! classification (`is_dead_ublk_device_error`: recover AND start both
//! ENODEV). On a live device DEL_DEV would rip the disk out from under a
//! mounted filesystem. `FLINT_UBLK_DEL_DEV=0` opts out entirely, turning
//! the path back into the log-runbook-and-reboot behavior.
//!
//! The control-plane ABI (`struct ublksrv_ctrl_cmd`, the ioctl-style
//! opcode encoding, SQE128) is pinned by tests below so a toolchain or
//! kernel-header drift breaks the build loudly, not the node quietly.

/// Control device node (ublk_drv creates it at module load).
pub const UBLK_CONTROL: &str = "/dev/ublk-control";

/// `_IOWR('u', 0x05, struct ublksrv_ctrl_cmd)` — the ioctl-encoded
/// DEL_DEV opcode modern kernels expect (UBLK_F_CMD_IOCTL_ENCODE era).
pub const UBLK_U_CMD_DEL_DEV: u32 = 0xC020_7505;

/// Pre-ioctl-encoding opcode; kernels that don't know the encoded form
/// answer EOPNOTSUPP to it and we retry with this.
pub const UBLK_CMD_DEL_DEV_LEGACY: u32 = 0x05;

/// Mirror of the kernel's `struct ublksrv_ctrl_cmd`
/// (include/uapi/linux/ublk_cmd.h) — 32 bytes, layout pinned by tests.
#[repr(C)]
pub struct UblkCtrlCmd {
    pub dev_id: u32,
    pub queue_id: u16,
    pub len: u16,
    pub addr: u64,
    pub data: [u64; 1],
    pub dev_path_len: u16,
    pub pad: u16,
    pub reserved: u32,
}

/// DEL_DEV payload in the 80-byte `uring_cmd` slot of an SQE128 entry:
/// only `dev_id` matters; `queue_id` is -1 per the kernel contract for
/// non-queue commands.
pub fn del_dev_cmd(dev_id: u32) -> [u8; 80] {
    let mut cmd = [0u8; 80];
    cmd[0..4].copy_from_slice(&dev_id.to_le_bytes());
    cmd[4..6].copy_from_slice(&u16::MAX.to_le_bytes());
    cmd
}

/// Gate: on unless FLINT_UBLK_DEL_DEV says otherwise. Firing only after
/// the double-ENODEV classification makes default-on safe — the device
/// is unusable by definition when we get here.
pub fn escape_enabled() -> bool {
    escape_enabled_from(std::env::var("FLINT_UBLK_DEL_DEV").ok().as_deref())
}

pub fn escape_enabled_from(v: Option<&str>) -> bool {
    !matches!(v.map(str::trim), Some("0") | Some("false") | Some("off"))
}

/// Delete kernel ublk device `dev_id` via uring_cmd on /dev/ublk-control.
/// Blocking (the kernel may wait for device teardown) — call from
/// spawn_blocking with a timeout.
#[cfg(target_os = "linux")]
pub fn del_dev(dev_id: u32) -> std::io::Result<()> {
    use io_uring::{cqueue, opcode, squeue, types, IoUring};
    use std::os::fd::AsRawFd;

    let ctrl = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(UBLK_CONTROL)?;
    // ublk control commands require extended SQEs (the 32-byte cmd does
    // not fit a plain SQE's 16-byte slot); Entry128 sets SQE128 for us.
    let mut ring: IoUring<squeue::Entry128, cqueue::Entry> =
        IoUring::builder().build(4)?;

    for cmd_op in [UBLK_U_CMD_DEL_DEV, UBLK_CMD_DEL_DEV_LEGACY] {
        let sqe = opcode::UringCmd80::new(types::Fd(ctrl.as_raw_fd()), cmd_op)
            .cmd(del_dev_cmd(dev_id))
            .build();
        unsafe {
            ring.submission()
                .push(&sqe)
                .map_err(|e| std::io::Error::other(format!("sqe push: {e}")))?;
        }
        ring.submit_and_wait(1)?;
        let cqe = ring
            .completion()
            .next()
            .ok_or_else(|| std::io::Error::other("submit_and_wait(1) returned without a cqe"))?;
        let res = cqe.result();
        if res >= 0 {
            return Ok(());
        }
        if res != -libc::EOPNOTSUPP {
            return Err(std::io::Error::from_raw_os_error(-res));
        }
        // EOPNOTSUPP: this kernel doesn't speak the ioctl encoding —
        // loop retries with the legacy opcode.
    }
    Err(std::io::Error::from_raw_os_error(libc::EOPNOTSUPP))
}

#[cfg(not(target_os = "linux"))]
pub fn del_dev(_dev_id: u32) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "ublk control requires Linux io_uring",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Recompute _IOWR('u', 0x05, sizeof(struct ublksrv_ctrl_cmd)) from
    /// first principles so the pinned constant can't drift silently.
    #[test]
    fn ioctl_encoding_is_pinned() {
        const IOC_WRITE: u32 = 1;
        const IOC_READ: u32 = 2;
        let iowr = ((IOC_READ | IOC_WRITE) << 30)
            | ((core::mem::size_of::<UblkCtrlCmd>() as u32) << 16)
            | ((b'u' as u32) << 8)
            | 0x05;
        assert_eq!(UBLK_U_CMD_DEL_DEV, iowr);
        assert_eq!(UBLK_U_CMD_DEL_DEV, 0xC020_7505);
        assert_eq!(UBLK_CMD_DEL_DEV_LEGACY, 0x05);
    }

    /// Field-for-field pin of the kernel ABI. A silent size/offset shift
    /// here would corrupt the command on the wire.
    #[test]
    fn ctrl_cmd_layout_matches_kernel_abi() {
        use core::mem::{offset_of, size_of};
        assert_eq!(size_of::<UblkCtrlCmd>(), 32);
        assert_eq!(offset_of!(UblkCtrlCmd, dev_id), 0);
        assert_eq!(offset_of!(UblkCtrlCmd, queue_id), 4);
        assert_eq!(offset_of!(UblkCtrlCmd, len), 6);
        assert_eq!(offset_of!(UblkCtrlCmd, addr), 8);
        assert_eq!(offset_of!(UblkCtrlCmd, data), 16);
        assert_eq!(offset_of!(UblkCtrlCmd, dev_path_len), 24);
        assert_eq!(offset_of!(UblkCtrlCmd, pad), 26);
        assert_eq!(offset_of!(UblkCtrlCmd, reserved), 28);
    }

    #[test]
    fn del_dev_payload_sets_dev_id_and_no_queue() {
        let c = del_dev_cmd(7);
        assert_eq!(&c[0..4], &7u32.to_le_bytes());
        assert_eq!(&c[4..6], &u16::MAX.to_le_bytes());
        assert!(c[6..].iter().all(|b| *b == 0));
    }

    #[test]
    fn escape_gate_defaults_on_opts_out() {
        assert!(escape_enabled_from(None));
        assert!(escape_enabled_from(Some("1")));
        assert!(escape_enabled_from(Some("true")));
        assert!(!escape_enabled_from(Some("0")));
        assert!(!escape_enabled_from(Some("false")));
        assert!(!escape_enabled_from(Some("off")));
        assert!(!escape_enabled_from(Some(" 0 ")));
    }
}
