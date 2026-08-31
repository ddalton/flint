//! One RPC-record ingress for every lane.
//!
//! Marker parse, fragment reassembly, the whole-record size cap, the
//! pooled buffer for payload-bearing records, and the two idle
//! deadlines (between records: an ordinary close; mid-record: a
//! malformed exchange) — the mechanics every RPC listener needs and
//! that must behave identically on all of them.
//!
//! Before this module the shared lane and the DS each carried their
//! own copy, and they had drifted exactly the way copies do: the
//! fragment-reassembly fix (`85184494`) reached only the shared lane —
//! the DS dispatched every fragment as a whole record until this
//! extraction — and the pooled-ingress fix (`e399d031`) took a year of
//! wall-clock and a measurement campaign to make the same trip.

use bytes::{Bytes, BytesMut};
use tokio::io::{AsyncRead, AsyncReadExt};
use tracing::{debug, warn};

/// Ceiling on one assembled RPC record. Applies to the SUM of a
/// record's fragments, not to any single one — bounding the fragment
/// while accepting unlimited fragments is not a bound at all.
pub(crate) const MAX_RPC_RECORD_BYTES: usize = 4 * 1024 * 1024;

/// Records larger than this read into a pooled buffer instead of the
/// connection's `BytesMut`. Below it the `BytesMut` amortizes
/// allocations across records; above it (the BufReader's capacity)
/// every record forced a fresh allocation, because `split().freeze()`
/// donates the storage to the request. A 1 MiB WRITE payload sits far
/// above this line.
pub(crate) const POOLED_RECORD_MIN: usize = 128 * 1024;

/// Counts records that took the pooled ingress path — the anti-vacuity
/// guard for the test that sends one: a green round-trip proves nothing
/// if the record quietly took the `BytesMut` path instead.
#[cfg(test)]
pub(crate) static POOLED_RECORDS_FOR_TEST: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// What [`RecordReader::next`] came back with.
pub(crate) enum NextRecord {
    /// A complete RPC record, fragments already assembled.
    Record(Bytes),
    /// Clean EOF at a record boundary — the peer hung up between
    /// requests. (EOF *inside* a record is an error, not this.)
    Closed,
    /// The between-records idle deadline passed with nothing to read.
    /// An ordinary close, not an error: the peer simply had nothing to
    /// say for the whole window.
    IdleClosed,
}

/// Per-connection reassembly state plus the connection's log label.
pub(crate) struct RecordReader {
    buf: BytesMut,
    /// Who this connection is, for log lines — e.g.
    /// `"[NFS_SERVER] Connection #3 from 1.2.3.4:5"`.
    label: String,
}

impl RecordReader {
    pub(crate) fn new(label: String) -> Self {
        Self {
            buf: BytesMut::with_capacity(128 * 1024),
            label,
        }
    }

    /// Read ONE complete record (assembling fragments as needed).
    ///
    /// `idle_timeout` bounds both waits: between records it turns into
    /// [`NextRecord::IdleClosed`]; mid-record (a continuation marker or
    /// a promised payload that never arrives) it is an error — the peer
    /// promised bytes and did not deliver.
    pub(crate) async fn next<R>(
        &mut self,
        reader: &mut R,
        idle_timeout: Option<std::time::Duration>,
    ) -> std::io::Result<NextRecord>
    where
        R: AsyncRead + Unpin,
    {
        loop {
            let assembled = self.buf.len();

            // ---- record marker ----
            let mut marker_buf = [0u8; 4];
            let marker_res = match idle_timeout {
                Some(d) => match tokio::time::timeout(d, reader.read_exact(&mut marker_buf)).await
                {
                    Ok(r) => r,
                    Err(_elapsed) if assembled == 0 => return Ok(NextRecord::IdleClosed),
                    Err(_elapsed) => {
                        warn!(
                            "⏱️  {} stalled mid-record — {} bytes assembled, continuation \
                             marker never arrived within {:?} (FLINT_NFS_IDLE_TIMEOUT_SECS)",
                            self.label, assembled, d
                        );
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "RPC continuation stalled past the idle deadline",
                        ));
                    }
                },
                None => reader.read_exact(&mut marker_buf).await,
            };
            match marker_res {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof && assembled == 0 => {
                    return Ok(NextRecord::Closed);
                }
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    // The old loop logged this as a CLEAN close; a peer
                    // dying mid-record is not one.
                    warn!(
                        "❌ {} closed mid-record with {} bytes assembled",
                        self.label, assembled
                    );
                    return Err(e);
                }
                Err(e) => return Err(e),
            }

            let marker = u32::from_be_bytes(marker_buf);
            let is_last = (marker & 0x8000_0000) != 0;
            let length = (marker & 0x7FFF_FFFF) as usize;

            debug!(
                "📊 {} RPC marker: is_last={}, length={} bytes",
                self.label, is_last, length
            );

            if assembled + length > MAX_RPC_RECORD_BYTES {
                warn!(
                    "❌ {} oversized RPC record: {} + {} bytes exceeds {} \
                     (a record may arrive in fragments; the limit is on the whole)",
                    self.label, assembled, length, MAX_RPC_RECORD_BYTES
                );
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "RPC record too large",
                ));
            }

            // ---- payload ----
            //
            // Sized with `resize`, never `unsafe set_len`: if
            // `read_exact` errors partway, the next user of `buf` must
            // not observe uninitialized memory. A payload-bearing
            // single-fragment record takes a POOLED buffer instead —
            // `split().freeze()` donates `buf`'s storage to the
            // request, so above the BufReader's capacity the `BytesMut`
            // path paid a fresh allocation whose pages the kernel
            // zeroes on first touch (`__pi_clear_page` at 3.5% of
            // system in the write-path profile). Fragmented records
            // keep the `BytesMut` path: fragments must append to what
            // is assembled.
            let mut pooled_buf: Option<crate::nfs::read_pool::PooledMut> =
                if is_last && assembled == 0 && length > POOLED_RECORD_MIN {
                    #[cfg(test)]
                    POOLED_RECORDS_FOR_TEST.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    Some(crate::nfs::read_pool::take_mut(length))
                } else {
                    self.buf.resize(assembled + length, 0);
                    None
                };

            let dst: &mut [u8] = match pooled_buf.as_mut() {
                Some(pb) => &mut pb[..length],
                None => &mut self.buf[assembled..assembled + length],
            };
            // Mid-record deadline: unlike the idle case above this is a
            // malformed exchange — the peer promised `length` bytes and
            // did not deliver — so it is an error, not a quiet close.
            match idle_timeout {
                Some(d) => match tokio::time::timeout(d, reader.read_exact(dst)).await {
                    Ok(r) => {
                        r?;
                    }
                    Err(_elapsed) => {
                        warn!(
                            "⏱️  {} stalled mid-request — promised {} bytes, did not \
                             deliver within {:?} (FLINT_NFS_IDLE_TIMEOUT_SECS)",
                            self.label, length, d
                        );
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "RPC payload read stalled past the idle deadline",
                        ));
                    }
                },
                None => {
                    reader.read_exact(dst).await?;
                }
            }

            if !is_last {
                debug!(
                    "📎 {} RPC fragment: {} bytes, record now {} — awaiting continuation",
                    self.label,
                    length,
                    self.buf.len()
                );
                continue;
            }

            let record = match pooled_buf.take() {
                Some(pb) => pb.freeze(length),
                None => {
                    if self.buf.is_empty() {
                        warn!("⚠️  {} zero-length RPC record, ignoring", self.label);
                        continue;
                    }
                    self.buf.split().freeze()
                }
            };
            return Ok(NextRecord::Record(record));
        }
    }
}
