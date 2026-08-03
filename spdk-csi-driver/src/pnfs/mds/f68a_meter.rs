//! F68a: the MDS data-path meter.
//!
//! A pNFS client that is healthy does (approximately) zero DATA I/O
//! through the MDS — bytes belong on the DSes. But the MDS has TWO
//! lanes that will happily carry client data anyway, and before this
//! meter NEITHER was visible at default log levels:
//!
//! 1. **Serve** — the file has no pinned placement (the client never
//!    obtained a layout, so no placement was ever minted) and the MDS
//!    treats it as a plain local file: writes land in the stub
//!    (making it dense), reads serve those bytes back. Correct data,
//!    plausible throughput, zero log lines. This is the lane the
//!    runbg F68c flip actually rode.
//! 2. **Proxy** (F66) — the file IS striped and the MDS relays I/O to
//!    the stripes via DsControl. Logged per-op at debug only (info
//!    would flood at line rate — the flip ran ~3000 ops/s).
//!
//! Both lanes are metered here with relaxed atomics (a handful of
//! uncontended fetch_adds per RPC — unmeasurable next to an NFS
//! round-trip), plus layout-op counters so a flip window shows
//! whether the client STOPPED ASKING for layouts (client-side latch)
//! or was REFUSED them (server-side). A reporter task on the MDS
//! turns deltas into one log line per interval: silence when idle,
//! INFO for modest fallback traffic, WARN when data flows through the
//! MDS while the DS fleet is healthy — the F68 signature.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

#[derive(Debug, Default)]
pub struct DataPathMeter {
    pub served_read_ops: AtomicU64,
    pub served_read_bytes: AtomicU64,
    pub served_write_ops: AtomicU64,
    pub served_write_bytes: AtomicU64,
    pub proxy_read_ops: AtomicU64,
    pub proxy_read_bytes: AtomicU64,
    pub proxy_write_ops: AtomicU64,
    pub proxy_write_bytes: AtomicU64,
    pub layoutget_granted: AtomicU64,
    pub layoutget_refused: AtomicU64,
    pub layouts_returned: AtomicU64,
}

impl DataPathMeter {
    pub fn served_read(&self, bytes: u64) {
        self.served_read_ops.fetch_add(1, Relaxed);
        self.served_read_bytes.fetch_add(bytes, Relaxed);
    }
    pub fn served_write(&self, bytes: u64) {
        self.served_write_ops.fetch_add(1, Relaxed);
        self.served_write_bytes.fetch_add(bytes, Relaxed);
    }
    pub fn proxy_read(&self, bytes: u64) {
        self.proxy_read_ops.fetch_add(1, Relaxed);
        self.proxy_read_bytes.fetch_add(bytes, Relaxed);
    }
    pub fn proxy_write(&self, bytes: u64) {
        self.proxy_write_ops.fetch_add(1, Relaxed);
        self.proxy_write_bytes.fetch_add(bytes, Relaxed);
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            served_read_ops: self.served_read_ops.load(Relaxed),
            served_read_bytes: self.served_read_bytes.load(Relaxed),
            served_write_ops: self.served_write_ops.load(Relaxed),
            served_write_bytes: self.served_write_bytes.load(Relaxed),
            proxy_read_ops: self.proxy_read_ops.load(Relaxed),
            proxy_read_bytes: self.proxy_read_bytes.load(Relaxed),
            proxy_write_ops: self.proxy_write_ops.load(Relaxed),
            proxy_write_bytes: self.proxy_write_bytes.load(Relaxed),
            layoutget_granted: self.layoutget_granted.load(Relaxed),
            layoutget_refused: self.layoutget_refused.load(Relaxed),
            layouts_returned: self.layouts_returned.load(Relaxed),
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Snapshot {
    pub served_read_ops: u64,
    pub served_read_bytes: u64,
    pub served_write_ops: u64,
    pub served_write_bytes: u64,
    pub proxy_read_ops: u64,
    pub proxy_read_bytes: u64,
    pub proxy_write_ops: u64,
    pub proxy_write_bytes: u64,
    pub layoutget_granted: u64,
    pub layoutget_refused: u64,
    pub layouts_returned: u64,
}

impl Snapshot {
    /// Counter deltas since `prev` (both from the same meter; counters
    /// are monotonic so saturating_sub only papers over restarts).
    pub fn delta_since(&self, prev: &Snapshot) -> Snapshot {
        Snapshot {
            served_read_ops: self.served_read_ops.saturating_sub(prev.served_read_ops),
            served_read_bytes: self.served_read_bytes.saturating_sub(prev.served_read_bytes),
            served_write_ops: self.served_write_ops.saturating_sub(prev.served_write_ops),
            served_write_bytes: self.served_write_bytes.saturating_sub(prev.served_write_bytes),
            proxy_read_ops: self.proxy_read_ops.saturating_sub(prev.proxy_read_ops),
            proxy_read_bytes: self.proxy_read_bytes.saturating_sub(prev.proxy_read_bytes),
            proxy_write_ops: self.proxy_write_ops.saturating_sub(prev.proxy_write_ops),
            proxy_write_bytes: self.proxy_write_bytes.saturating_sub(prev.proxy_write_bytes),
            layoutget_granted: self.layoutget_granted.saturating_sub(prev.layoutget_granted),
            layoutget_refused: self.layoutget_refused.saturating_sub(prev.layoutget_refused),
            layouts_returned: self.layouts_returned.saturating_sub(prev.layouts_returned),
        }
    }

    pub fn is_zero(&self) -> bool {
        *self == Snapshot::default()
    }

    /// Client data bytes that crossed the MDS (both lanes, both
    /// directions) — the quantity that should be ~0 on a healthy
    /// pNFS fleet.
    pub fn mds_data_bytes(&self) -> u64 {
        self.served_read_bytes
            + self.served_write_bytes
            + self.proxy_read_bytes
            + self.proxy_write_bytes
    }

    /// One-line human rendering for the reporter.
    pub fn render(&self) -> String {
        format!(
            "served r {}op/{}MiB w {}op/{}MiB · proxy r {}op/{}MiB w {}op/{}MiB · layoutget +{}/-{} · layoutreturn {}",
            self.served_read_ops,
            self.served_read_bytes >> 20,
            self.served_write_ops,
            self.served_write_bytes >> 20,
            self.proxy_read_ops,
            self.proxy_read_bytes >> 20,
            self.proxy_write_ops,
            self.proxy_write_bytes >> 20,
            self.layoutget_granted,
            self.layoutget_refused,
            self.layouts_returned,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering::Relaxed;

    #[test]
    fn deltas_track_only_new_traffic() {
        let m = DataPathMeter::default();
        m.served_write(1 << 20);
        m.proxy_read(2 << 20);
        let s1 = m.snapshot();
        assert_eq!(s1.mds_data_bytes(), 3 << 20);

        m.served_read(4 << 20);
        m.layoutget_granted.fetch_add(2, Relaxed);
        let d = m.snapshot().delta_since(&s1);
        assert_eq!(d.served_read_ops, 1);
        assert_eq!(d.served_read_bytes, 4 << 20);
        assert_eq!(d.served_write_ops, 0, "old traffic must not reappear in the delta");
        assert_eq!(d.layoutget_granted, 2);
        assert_eq!(d.mds_data_bytes(), 4 << 20);
        assert!(!d.is_zero());
    }

    #[test]
    fn quiet_interval_is_zero() {
        let m = DataPathMeter::default();
        m.proxy_write(7);
        let s1 = m.snapshot();
        let d = m.snapshot().delta_since(&s1);
        assert!(d.is_zero(), "no new traffic ⇒ the reporter must stay silent");
    }

    /// Layout-op deltas alone (a client asking for / returning
    /// layouts) must NOT count as MDS data traffic — the WARN
    /// threshold keys on data bytes only.
    #[test]
    fn layout_ops_are_not_data() {
        let m = DataPathMeter::default();
        m.layoutget_granted.fetch_add(5, Relaxed);
        m.layouts_returned.fetch_add(5, Relaxed);
        let s = m.snapshot();
        assert_eq!(s.mds_data_bytes(), 0);
        assert!(!s.is_zero());
    }
}
