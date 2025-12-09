// NFSv4 XDR Encoding/Decoding Extensions
//
// Extends the base XDR implementation with NFSv4-specific types:
// - StateId, FileHandle, SessionId, ClientId
// - Attribute bitmaps
// - Discriminated unions (for READ_PLUS, etc.)
//
// Uses existing XdrEncoder/XdrDecoder from parent module

use super::protocol::*;
use crate::nfs::xdr::{XdrEncoder, XdrDecoder};
use bytes::Bytes;

/// NFSv4-specific XDR encoding extensions
pub trait Nfs4XdrEncoder {
    /// Encode a StateId (seqid + 12-byte opaque)
    fn encode_stateid(&mut self, stateid: &StateId);

    /// Encode an NFSv4 file handle (variable-length opaque, max 128 bytes)
    fn encode_filehandle(&mut self, fh: &Nfs4FileHandle);

    /// Encode a SessionId (16-byte fixed opaque)
    fn encode_sessionid(&mut self, sid: &SessionId);

    /// Encode an attribute bitmap (variable-length u32 array)
    fn encode_bitmap(&mut self, bitmap: &[u32]);

    /// Encode NFSv4 status code
    fn encode_status(&mut self, status: Nfs4Status);

    /// Encode a verifier (8-byte fixed opaque)
    fn encode_verifier(&mut self, verifier: &[u8; 8]);

    /// Encode a ClientId (verifier + opaque ID)
    fn encode_clientid(&mut self, client_id: &ClientId);

    /// Encode a discriminated union (discriminant + arm-specific data)
    /// Returns true if caller should encode the arm data
    fn encode_union_discriminant(&mut self, discriminant: u32) -> bool;
}

impl Nfs4XdrEncoder for XdrEncoder {
    fn encode_stateid(&mut self, stateid: &StateId) {
        self.encode_u32(stateid.seqid);
        self.encode_fixed_opaque(&stateid.other);
    }

    fn encode_filehandle(&mut self, fh: &Nfs4FileHandle) {
        self.encode_opaque(&fh.data);
    }

    fn encode_sessionid(&mut self, sid: &SessionId) {
        self.encode_fixed_opaque(&sid.0);
    }

    fn encode_bitmap(&mut self, bitmap: &[u32]) {
        self.encode_u32(bitmap.len() as u32);
        for &word in bitmap {
            self.encode_u32(word);
        }
    }

    fn encode_status(&mut self, status: Nfs4Status) {
        self.encode_u32(status.to_u32());
    }

    fn encode_verifier(&mut self, verifier: &[u8; 8]) {
        self.encode_fixed_opaque(verifier);
    }

    fn encode_clientid(&mut self, client_id: &ClientId) {
        self.encode_u64(client_id.verifier);
        self.encode_opaque(&client_id.id);
    }

    fn encode_union_discriminant(&mut self, discriminant: u32) -> bool {
        self.encode_u32(discriminant);
        true // Caller should encode arm data
    }
}

/// NFSv4-specific XDR decoding extensions
pub trait Nfs4XdrDecoder {
    /// Decode a StateId (seqid + 12-byte opaque)
    fn decode_stateid(&mut self) -> Result<StateId, String>;

    /// Decode an NFSv4 file handle (variable-length opaque, max 128 bytes)
    fn decode_filehandle(&mut self) -> Result<Nfs4FileHandle, String>;

    /// Decode a SessionId (16-byte fixed opaque)
    fn decode_sessionid(&mut self) -> Result<SessionId, String>;

    /// Decode an attribute bitmap (variable-length u32 array)
    fn decode_bitmap(&mut self) -> Result<Vec<u32>, String>;

    /// Decode NFSv4 status code
    fn decode_status(&mut self) -> Result<Nfs4Status, String>;

    /// Decode a verifier (8-byte fixed opaque)
    fn decode_verifier(&mut self) -> Result<[u8; 8], String>;

    /// Decode a ClientId (verifier + opaque ID)
    fn decode_clientid(&mut self) -> Result<ClientId, String>;

    /// Decode a discriminated union discriminant
    /// Returns the discriminant value for switch/match
    fn decode_union_discriminant(&mut self) -> Result<u32, String>;
}

impl Nfs4XdrDecoder for XdrDecoder {
    fn decode_stateid(&mut self) -> Result<StateId, String> {
        let seqid = self.decode_u32()?;
        let other_vec = self.decode_fixed_opaque(12)?;
        let mut other = [0u8; 12];
        other.copy_from_slice(&other_vec);
        Ok(StateId { seqid, other })
    }

    fn decode_filehandle(&mut self) -> Result<Nfs4FileHandle, String> {
        let data = self.decode_opaque()?;
        if data.len() > Nfs4FileHandle::MAX_SIZE {
            return Err("File handle too large".to_string());
        }
        // Convert Bytes to Vec<u8>
        Ok(Nfs4FileHandle { data: data.to_vec() })
    }

    fn decode_sessionid(&mut self) -> Result<SessionId, String> {
        let data = self.decode_fixed_opaque(16)?;
        let mut id = [0u8; 16];
        id.copy_from_slice(&data);
        Ok(SessionId(id))
    }

    fn decode_bitmap(&mut self) -> Result<Vec<u32>, String> {
        let count = self.decode_u32()? as usize;
        let mut bitmap = Vec::with_capacity(count);
        for _ in 0..count {
            bitmap.push(self.decode_u32()?);
        }
        Ok(bitmap)
    }

    fn decode_status(&mut self) -> Result<Nfs4Status, String> {
        let status_code = self.decode_u32()?;
        Ok(Nfs4Status::from_u32(status_code))
    }

    fn decode_verifier(&mut self) -> Result<[u8; 8], String> {
        let data = self.decode_fixed_opaque(8)?;
        let mut verifier = [0u8; 8];
        verifier.copy_from_slice(&data);
        Ok(verifier)
    }

    fn decode_clientid(&mut self) -> Result<ClientId, String> {
        let verifier = self.decode_u64()?;
        let id = self.decode_opaque()?;
        // Convert Bytes to Vec<u8>
        Ok(ClientId { verifier, id: id.to_vec() })
    }

    fn decode_union_discriminant(&mut self) -> Result<u32, String> {
        self.decode_u32()
    }
}

/// Helper for encoding attribute values
pub struct AttrEncoder {
    encoder: XdrEncoder,
}

impl AttrEncoder {
    pub fn new() -> Self {
        Self {
            encoder: XdrEncoder::new(),
        }
    }

    /// Encode a u64 attribute (e.g., size, fileid)
    pub fn encode_u64(&mut self, value: u64) {
        self.encoder.encode_u64(value);
    }

    /// Encode a u32 attribute (e.g., mode, numlinks)
    pub fn encode_u32(&mut self, value: u32) {
        self.encoder.encode_u32(value);
    }

    /// Encode a string attribute (e.g., owner, group)
    pub fn encode_string(&mut self, value: &str) {
        self.encoder.encode_string(value);
    }

    /// Encode a timespec attribute
    pub fn encode_nfstime4(&mut self, seconds: i64, nseconds: u32) {
        self.encoder.encode_u64(seconds as u64);
        self.encoder.encode_u32(nseconds);
    }

    /// Encode file type
    pub fn encode_filetype(&mut self, ftype: Nfs4FileType) {
        self.encoder.encode_u32(ftype as u32);
    }

    /// Get the encoded bytes
    pub fn finish(self) -> Bytes {
        self.encoder.finish()
    }
}

/// Helper for decoding attribute values
pub struct AttrDecoder {
    decoder: XdrDecoder,
}

impl AttrDecoder {
    pub fn new(data: Bytes) -> Self {
        Self {
            decoder: XdrDecoder::new(data),
        }
    }

    /// Decode a u64 attribute
    pub fn decode_u64(&mut self) -> Result<u64, String> {
        self.decoder.decode_u64()
    }

    /// Decode a u32 attribute
    pub fn decode_u32(&mut self) -> Result<u32, String> {
        self.decoder.decode_u32()
    }

    /// Decode a string attribute
    pub fn decode_string(&mut self) -> Result<String, String> {
        self.decoder.decode_string()
    }

    /// Decode a timespec attribute
    pub fn decode_nfstime4(&mut self) -> Result<(i64, u32), String> {
        let seconds = self.decoder.decode_u64()? as i64;
        let nseconds = self.decoder.decode_u32()?;
        Ok((seconds, nseconds))
    }

    /// Decode file type
    pub fn decode_filetype(&mut self) -> Result<Nfs4FileType, String> {
        let val = self.decoder.decode_u32()?;
        match val {
            1 => Ok(Nfs4FileType::Regular),
            2 => Ok(Nfs4FileType::Directory),
            3 => Ok(Nfs4FileType::BlockDevice),
            4 => Ok(Nfs4FileType::CharDevice),
            5 => Ok(Nfs4FileType::Symlink),
            6 => Ok(Nfs4FileType::Socket),
            7 => Ok(Nfs4FileType::Fifo),
            8 => Ok(Nfs4FileType::AttrDir),
            9 => Ok(Nfs4FileType::NamedAttr),
            _ => Err("Invalid file type".to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stateid_encoding() {
        let mut enc = XdrEncoder::new();
        let stateid = StateId {
            seqid: 42,
            other: [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12],
        };

        enc.encode_stateid(&stateid);
        let bytes = enc.finish();

        let mut dec = XdrDecoder::new(bytes);
        let decoded = dec.decode_stateid().unwrap();

        assert_eq!(decoded.seqid, 42);
        assert_eq!(decoded.other, [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
    }

    #[test]
    fn test_filehandle_encoding() {
        let mut enc = XdrEncoder::new();
        let fh = Nfs4FileHandle {
            data: vec![1, 2, 3, 4, 5],
        };

        enc.encode_filehandle(&fh);
        let bytes = enc.finish();

        let mut dec = XdrDecoder::new(bytes);
        let decoded = dec.decode_filehandle().unwrap();

        assert_eq!(decoded.data, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn test_sessionid_encoding() {
        let mut enc = XdrEncoder::new();
        let sid = SessionId([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]);

        enc.encode_sessionid(&sid);
        let bytes = enc.finish();

        let mut dec = XdrDecoder::new(bytes);
        let decoded = dec.decode_sessionid().unwrap();

        assert_eq!(decoded.0, [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]);
    }

    #[test]
    fn test_bitmap_encoding() {
        let mut enc = XdrEncoder::new();
        let bitmap = vec![0x0000_000F, 0xFFFF_0000];

        enc.encode_bitmap(&bitmap);
        let bytes = enc.finish();

        let mut dec = XdrDecoder::new(bytes);
        let decoded = dec.decode_bitmap().unwrap();

        assert_eq!(decoded, vec![0x0000_000F, 0xFFFF_0000]);
    }

    #[test]
    fn test_status_encoding() {
        let mut enc = XdrEncoder::new();
        enc.encode_status(Nfs4Status::Ok);
        let bytes = enc.finish();

        let mut dec = XdrDecoder::new(bytes);
        let decoded = dec.decode_status().unwrap();

        assert_eq!(decoded, Nfs4Status::Ok);
    }
}
