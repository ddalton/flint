//! XDR (External Data Representation) encoding and decoding
//!
//! Implementation of RFC 4506 - XDR: External Data Representation Standard
//! https://datatracker.ietf.org/doc/html/rfc4506
//!
//! XDR is a standard for describing and encoding data. It's used by RPC and NFS.
//!
//! # Format
//!
//! - All integers are big-endian (network byte order)
//! - All data types are aligned on 4-byte boundaries
//! - Variable-length data is prefixed with length

use bytes::{Buf, BufMut, Bytes, BytesMut};

/// XDR encoder - builds XDR-encoded messages
pub struct XdrEncoder {
    buf: BytesMut,
}

impl XdrEncoder {
    pub fn new() -> Self {
        Self {
            buf: BytesMut::with_capacity(8192),
        }
    }

    /// Encode a 32-bit unsigned integer
    pub fn encode_u32(&mut self, val: u32) {
        self.buf.put_u32(val);
    }

    /// Encode a 64-bit unsigned integer
    pub fn encode_u64(&mut self, val: u64) {
        self.buf.put_u64(val);
    }

    /// Encode a boolean (encoded as u32: 0 = false, 1 = true)
    pub fn encode_bool(&mut self, val: bool) {
        self.buf.put_u32(if val { 1 } else { 0 });
    }

    /// Encode opaque data (variable-length byte array)
    /// Format: length (u32) + data + padding to 4-byte boundary
    pub fn encode_opaque(&mut self, data: &[u8]) {
        let len = data.len() as u32;
        self.buf.put_u32(len);
        self.buf.put_slice(data);

        // Pad to 4-byte boundary
        let padding = (4 - (len % 4)) % 4;
        for _ in 0..padding {
            self.buf.put_u8(0);
        }
    }

    /// Encode a string (same as opaque data)
    pub fn encode_string(&mut self, s: &str) {
        self.encode_opaque(s.as_bytes());
    }

    /// Encode fixed-length opaque data (no length prefix)
    pub fn encode_fixed_opaque(&mut self, data: &[u8]) {
        self.buf.put_slice(data);

        // Pad to 4-byte boundary
        let padding = (4 - (data.len() % 4)) % 4;
        for _ in 0..padding {
            self.buf.put_u8(0);
        }
    }

    /// Encode an optional value (discriminated union)
    pub fn encode_option<T, F>(&mut self, opt: Option<T>, encode_fn: F)
    where
        F: FnOnce(&mut Self, T),
    {
        match opt {
            Some(val) => {
                self.encode_bool(true);
                encode_fn(self, val);
            }
            None => {
                self.encode_bool(false);
            }
        }
    }

    /// Encode an array with length prefix
    pub fn encode_array<T, F>(&mut self, items: &[T], encode_fn: F)
    where
        F: Fn(&mut Self, &T),
    {
        self.encode_u32(items.len() as u32);
        for item in items {
            encode_fn(self, item);
        }
    }

    /// Get the encoded bytes
    pub fn finish(self) -> Bytes {
        self.buf.freeze()
    }

    /// Get current buffer length
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Append raw bytes directly (no length prefix, no padding)
    /// Used when the data is already properly XDR-encoded
    pub fn append_raw(&mut self, data: &[u8]) {
        self.buf.put_slice(data);
    }

    /// Append raw bytes without any encoding (no length prefix, no padding)
    /// Use this for RPC procedure results that are already XDR-encoded
    pub fn append_bytes(&mut self, data: &[u8]) {
        self.buf.put_slice(data);
    }
}

impl Default for XdrEncoder {
    fn default() -> Self {
        Self::new()
    }
}

/// XDR decoder - parses XDR-encoded messages
pub struct XdrDecoder {
    buf: Bytes,
}

impl XdrDecoder {
    pub fn new(buf: Bytes) -> Self {
        Self { buf }
    }

    /// Decode a 32-bit unsigned integer
    pub fn decode_u32(&mut self) -> Result<u32, String> {
        if self.buf.remaining() < 4 {
            return Err("Not enough data for u32".to_string());
        }
        Ok(self.buf.get_u32())
    }

    /// Decode a 64-bit unsigned integer
    pub fn decode_u64(&mut self) -> Result<u64, String> {
        if self.buf.remaining() < 8 {
            return Err("Not enough data for u64".to_string());
        }
        Ok(self.buf.get_u64())
    }

    /// Decode a boolean
    pub fn decode_bool(&mut self) -> Result<bool, String> {
        let val = self.decode_u32()?;
        Ok(val != 0)
    }

    /// Decode opaque data (variable-length byte array)
    pub fn decode_opaque(&mut self) -> Result<Bytes, String> {
        let len = self.decode_u32()? as usize;

        // Calculate padded length (round up to 4-byte boundary)
        let padded_len = (len + 3) & !3;

        if self.buf.remaining() < padded_len {
            return Err(format!("Not enough data for opaque (need {}, have {})",
                             padded_len, self.buf.remaining()));
        }

        let data = self.buf.copy_to_bytes(len);

        // Skip padding
        self.buf.advance(padded_len - len);

        Ok(data)
    }

    /// Decode a string
    pub fn decode_string(&mut self) -> Result<String, String> {
        let bytes = self.decode_opaque()?;
        String::from_utf8(bytes.to_vec())
            .map_err(|e| format!("Invalid UTF-8: {}", e))
    }

    /// Decode fixed-length opaque data
    pub fn decode_fixed_opaque(&mut self, len: usize) -> Result<Bytes, String> {
        // Calculate padded length
        let padded_len = (len + 3) & !3;

        if self.buf.remaining() < padded_len {
            return Err(format!("Not enough data for fixed opaque (need {}, have {})",
                             padded_len, self.buf.remaining()));
        }

        let data = self.buf.copy_to_bytes(len);

        // Skip padding
        self.buf.advance(padded_len - len);

        Ok(data)
    }

    /// Decode an optional value
    pub fn decode_option<T, F>(&mut self, decode_fn: F) -> Result<Option<T>, String>
    where
        F: FnOnce(&mut Self) -> Result<T, String>,
    {
        let present = self.decode_bool()?;
        if present {
            Ok(Some(decode_fn(self)?))
        } else {
            Ok(None)
        }
    }

    /// Decode an array
    pub fn decode_array<T, F>(&mut self, decode_fn: F) -> Result<Vec<T>, String>
    where
        F: Fn(&mut Self) -> Result<T, String>,
    {
        let count = self.decode_u32()? as usize;
        let mut result = Vec::with_capacity(count);

        for _ in 0..count {
            result.push(decode_fn(self)?);
        }

        Ok(result)
    }

    /// Get remaining bytes count
    pub fn remaining(&self) -> usize {
        self.buf.remaining()
    }

    /// Consume and return remaining bytes as a Bytes object
    pub fn into_remaining_bytes(mut self) -> Bytes {
        // Extract all remaining bytes into a fresh Vec, then convert to Bytes
        // This ensures we get a clean buffer with cursor at position 0
        let len = self.buf.remaining();
        if len == 0 {
            return Bytes::new();
        }

        let mut vec = Vec::with_capacity(len);
        while self.buf.has_remaining() {
            vec.push(self.buf.get_u8());
        }

        eprintln!("DEBUG into_remaining_bytes: extracted {} bytes into vec", vec.len());
        Bytes::from(vec)
    }

    /// Check if there's more data to decode
    pub fn has_remaining(&self) -> bool {
        self.buf.has_remaining()
    }

    /// Peek at the remaining buffer (for debugging)
    pub fn peek_bytes(&self, len: usize) -> &[u8] {
        let available = self.buf.chunk();
        &available[..len.min(available.len())]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_u32() {
        let mut enc = XdrEncoder::new();
        enc.encode_u32(42);
        enc.encode_u32(0xDEADBEEF);

        let bytes = enc.finish();
        let mut dec = XdrDecoder::new(bytes);

        assert_eq!(dec.decode_u32().unwrap(), 42);
        assert_eq!(dec.decode_u32().unwrap(), 0xDEADBEEF);
    }

    #[test]
    fn test_string() {
        let mut enc = XdrEncoder::new();
        enc.encode_string("hello");

        let bytes = enc.finish();
        let mut dec = XdrDecoder::new(bytes);

        assert_eq!(dec.decode_string().unwrap(), "hello");
    }

    #[test]
    fn test_opaque_padding() {
        // Test that padding is correct for various lengths
        for len in 0..8 {
            let mut enc = XdrEncoder::new();
            let data = vec![0xFF; len];
            enc.encode_opaque(&data);

            let bytes = enc.finish();

            // Length should be rounded up to 4-byte boundary
            let expected_len = 4 + ((len + 3) & !3);
            assert_eq!(bytes.len(), expected_len);
        }
    }

    #[test]
    fn test_option() {
        let mut enc = XdrEncoder::new();
        enc.encode_option(Some(42u32), |e, v| e.encode_u32(v));
        enc.encode_option(None::<u32>, |e, v| e.encode_u32(v));

        let bytes = enc.finish();
        let mut dec = XdrDecoder::new(bytes);

        assert_eq!(dec.decode_option(|d| d.decode_u32()).unwrap(), Some(42));
        assert_eq!(dec.decode_option(|d| d.decode_u32()).unwrap(), None);
    }
}
