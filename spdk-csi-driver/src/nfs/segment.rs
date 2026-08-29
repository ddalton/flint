//! One piece of a reply on its way to the wire.
//!
//! The reply path used to be `Vec<Bytes>`: a list of buffers the server
//! holds in memory. That is still what almost every segment is, but a
//! READ payload does not have to be — it can sit in a pipe, spliced
//! from the file and never entering userspace at all (see
//! [`crate::nfs::splice`]).
//!
//! This type is the seam that lets both travel the same path, so there
//! is ONE reply-assembly path rather than two. That matters here
//! specifically: `nfs::server_v4::frame_reply` exists because NULL and
//! COMPOUND once had separate framing that drifted apart, and a
//! conforming client got GARBAGE_ARGS on the GSS path for months.
//! Giving a spliced reply its own assembly path would recreate exactly
//! that failure mode.
//!
//! Today there is only [`Segment::Mem`]. The piped variant arrives with
//! the READ-path wiring; introducing the type first keeps that change to
//! behaviour alone, with no type churn mixed in.

use bytes::Bytes;

/// A contiguous run of reply bytes.
#[derive(Debug)]
pub enum Segment {
    /// Bytes the server already holds.
    Mem(Bytes),
}

impl Segment {
    /// Bytes this segment contributes to the RPC record marker.
    ///
    /// Every segment must be able to answer this WITHOUT its contents
    /// being readable: the record marker carries the total length and is
    /// written before any payload moves.
    pub fn len(&self) -> usize {
        match self {
            Segment::Mem(b) => b.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The bytes, for the surfaces that need them contiguous: GSS
    /// sealing, and the slot reply cache which must store the exact
    /// octets a replay will return (RFC 8881 §15.1.10.4).
    ///
    /// A payload that never enters userspace cannot satisfy either, so
    /// those two surfaces are the ones that must decline to splice in
    /// the first place rather than discovering it here.
    pub fn as_mem(&self) -> &Bytes {
        match self {
            Segment::Mem(b) => b,
        }
    }

    /// Consuming form of [`Segment::as_mem`].
    pub fn into_mem(self) -> Bytes {
        match self {
            Segment::Mem(b) => b,
        }
    }
}

impl From<Bytes> for Segment {
    fn from(b: Bytes) -> Self {
        Segment::Mem(b)
    }
}

/// Total wire length of a reply — what goes in the record marker.
pub fn total_len(segs: &[Segment]) -> usize {
    segs.iter().map(|s| s.len()).sum()
}
