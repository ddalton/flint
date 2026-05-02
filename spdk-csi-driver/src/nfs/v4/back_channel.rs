//! Per-connection back-channel writer.
//!
//! NFSv4.1 lets the *server* send RPCs back to the client over the same
//! TCP connection the client uses for its forward-channel requests
//! (RFC 8881 §2.10.3.1). Once a client issues
//! `BIND_CONN_TO_SESSION` with `conn_dir = BACKCHANNEL` or `BOTH`, the
//! server is permitted to issue callback CALLs (e.g. `CB_LAYOUTRECALL`,
//! `CB_RECALL` for delegations) on that connection.
//!
//! The actual TCP writer is owned by `handle_tcp_connection` in
//! `server_v4.rs`. To let *other* parts of the server (the
//! `CallbackManager`, the layout-recall fan-out on DS death) emit
//! callback frames on it, we wrap the writer in this type and store
//! `Arc<BackChannelWriter>` in a per-session registry on the
//! dispatcher. Both the main forward-reply path and the callback path
//! go through the same lock — RPC frames cannot interleave on the
//! wire, which is what ONC RPC framing requires.
//!
//! The lock is `tokio::sync::Mutex` (async-aware) so it can be held
//! across `await` without blocking the runtime. The contention rate
//! is low: forward replies write at the natural request-cadence,
//! callbacks fire at most once per layout change.

use bytes::Bytes;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::net::tcp::OwnedWriteHalf;
use tokio::sync::Mutex;

/// The buffered-write half of a single TCP connection, plus the
/// minimum API anyone outside `handle_tcp_connection` needs to push
/// an RPC frame onto the wire.
///
/// Held as `Arc<BackChannelWriter>` by:
///   * the connection's own main loop (for forward-reply writes), and
///   * any session that has bound this connection as a back-channel
///     (for callback writes).
///
/// Cloning the `Arc` is the only way to share it; the inner writer
/// itself is never duplicated.
pub struct BackChannelWriter {
    inner: Mutex<tokio::io::BufWriter<OwnedWriteHalf>>,
}

impl BackChannelWriter {
    pub fn new(buf_writer: tokio::io::BufWriter<OwnedWriteHalf>) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(buf_writer),
        })
    }

    /// Write a complete RPC frame to the wire — record marker
    /// (`0x80000000 | length`) followed by the body, then flush so the
    /// peer sees it immediately. Holds the per-connection lock for
    /// the duration of the marker + body + flush so a callback frame
    /// can never interleave with a forward-reply frame mid-write.
    pub async fn send_record(&self, payload: Bytes) -> std::io::Result<()> {
        let len = payload.len();
        if len > 0x7FFF_FFFF {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "RPC fragment exceeds 2 GiB",
            ));
        }
        let marker: u32 = 0x8000_0000 | (len as u32);

        let mut w = self.inner.lock().await;
        w.write_all(&marker.to_be_bytes()).await?;
        w.write_all(&payload).await?;
        w.flush().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    //! Integration-style tests for the writer alone. We can't construct
    //! a `BackChannelWriter` against a live `OwnedWriteHalf` without a
    //! TCP socket, so we wire up a `tokio::net::TcpListener` on
    //! 127.0.0.1:0, accept once on the server side, and drive the
    //! writer from the client side. This exercises the full path:
    //!   send_record → marker + payload + flush → bytes on the wire.

    use super::*;
    use tokio::io::AsyncReadExt;
    use tokio::net::{TcpListener, TcpStream};

    /// Helper: open a loopback pair (client OwnedWriteHalf wrapped in
    /// a BackChannelWriter, server's read half of the same connection).
    /// Returns (writer, server-side reader). The writer drives bytes
    /// onto the wire; the server-side reader is what we assert on.
    async fn pair() -> (Arc<BackChannelWriter>, tokio::net::tcp::OwnedReadHalf) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let connect = TcpStream::connect(addr);
        let accept = listener.accept();
        let (client_res, accept_res) = tokio::join!(connect, accept);
        let client_stream = client_res.unwrap();
        let (server_stream, _peer) = accept_res.unwrap();
        let (_client_read, client_write) = client_stream.into_split();
        let (server_read, _server_write) = server_stream.into_split();
        let writer = BackChannelWriter::new(
            tokio::io::BufWriter::with_capacity(64, client_write),
        );
        (writer, server_read)
    }

    #[tokio::test]
    async fn send_record_writes_marker_and_payload() {
        let (writer, mut reader) = pair().await;
        let payload = Bytes::from_static(b"hello");
        writer.send_record(payload).await.unwrap();

        // Read 4-byte marker + payload back from the server side.
        let mut marker = [0u8; 4];
        reader.read_exact(&mut marker).await.unwrap();
        let raw = u32::from_be_bytes(marker);
        // High bit set ("last fragment"), low 31 bits = length.
        assert_eq!(raw & 0x8000_0000, 0x8000_0000, "marker high bit");
        assert_eq!(raw & 0x7FFF_FFFF, 5,             "marker length");

        let mut buf = [0u8; 5];
        reader.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello");
    }

    #[tokio::test]
    async fn concurrent_sends_do_not_interleave() {
        // The Mutex inside BackChannelWriter must serialize so two
        // concurrent send_record calls produce two distinct, non-
        // interleaved record-marked frames on the wire. This is the
        // load-bearing guarantee for letting the main loop's forward
        // replies coexist with callback CB_LAYOUTRECALL frames.
        let (writer, mut reader) = pair().await;
        let w1 = Arc::clone(&writer);
        let w2 = Arc::clone(&writer);

        // Two long-ish payloads so a missing lock would visibly
        // interleave (the BufWriter would flush partial frames).
        let p1: Bytes = vec![b'A'; 256].into();
        let p2: Bytes = vec![b'B'; 512].into();
        let h1 = tokio::spawn(async move { w1.send_record(p1).await });
        let h2 = tokio::spawn(async move { w2.send_record(p2).await });
        h1.await.unwrap().unwrap();
        h2.await.unwrap().unwrap();

        // Read two frames; each must be either all-A or all-B (i.e. no
        // mix-and-match). Order between the two is unspecified — the
        // contract is that any single frame is internally consistent.
        for _ in 0..2 {
            let mut marker = [0u8; 4];
            reader.read_exact(&mut marker).await.unwrap();
            let len = (u32::from_be_bytes(marker) & 0x7FFF_FFFF) as usize;
            assert!(len == 256 || len == 512, "len {} not 256/512", len);
            let mut body = vec![0u8; len];
            reader.read_exact(&mut body).await.unwrap();
            // Body must be all the same byte — the test of non-interleave.
            let first = body[0];
            assert!(body.iter().all(|&b| b == first),
                "frame interleaved: head={} mixed", first);
        }
    }

    #[tokio::test]
    async fn closed_socket_surfaces_io_error() {
        // If the peer hangs up, the next send_record should fail with
        // an io::Error so the caller (CallbackManager, eventually) can
        // remove the dead writer from the registry.
        let (writer, reader) = pair().await;
        drop(reader); // server side closes
        // Write enough to actually push past kernel buffers; small
        // sends may be buffered locally and not surface the EPIPE.
        let big: Bytes = vec![0u8; 4 * 1024 * 1024].into();
        // First or second attempt should error; loop with a cap so we
        // don't hang on a broken assumption.
        let mut got_err = false;
        for _ in 0..4 {
            if writer.send_record(big.clone()).await.is_err() {
                got_err = true;
                break;
            }
        }
        assert!(got_err, "expected I/O error after peer close");
    }
}
