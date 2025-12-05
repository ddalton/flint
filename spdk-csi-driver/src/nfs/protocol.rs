//! NFSv3 Protocol Types
//!
//! Types and constants from RFC 1813 - NFS Version 3 Protocol Specification
//! https://datatracker.ietf.org/doc/html/rfc1813

use super::xdr::{XdrDecoder, XdrEncoder};
use bytes::Bytes;

/// NFS version 3 procedure numbers
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Procedure {
    Null = 0,
    GetAttr = 1,
    SetAttr = 2,
    Lookup = 3,
    Access = 4,
    ReadLink = 5,
    Read = 6,
    Write = 7,
    Create = 8,
    Mkdir = 9,
    Symlink = 10,
    Mknod = 11,
    Remove = 12,
    Rmdir = 13,
    Rename = 14,
    Link = 15,
    ReadDir = 16,
    ReadDirPlus = 17,
    FsStat = 18,
    FsInfo = 19,
    PathConf = 20,
    Commit = 21,
}

impl Procedure {
    pub fn from_u32(val: u32) -> Option<Self> {
        match val {
            0 => Some(Self::Null),
            1 => Some(Self::GetAttr),
            2 => Some(Self::SetAttr),
            3 => Some(Self::Lookup),
            4 => Some(Self::Access),
            5 => Some(Self::ReadLink),
            6 => Some(Self::Read),
            7 => Some(Self::Write),
            8 => Some(Self::Create),
            9 => Some(Self::Mkdir),
            10 => Some(Self::Symlink),
            11 => Some(Self::Mknod),
            12 => Some(Self::Remove),
            13 => Some(Self::Rmdir),
            14 => Some(Self::Rename),
            15 => Some(Self::Link),
            16 => Some(Self::ReadDir),
            17 => Some(Self::ReadDirPlus),
            18 => Some(Self::FsStat),
            19 => Some(Self::FsInfo),
            20 => Some(Self::PathConf),
            21 => Some(Self::Commit),
            _ => None,
        }
    }
}

// ACCESS procedure constants (RFC 1813 Section 3.3.4)
pub const ACCESS3_READ: u32 = 0x0001;
pub const ACCESS3_LOOKUP: u32 = 0x0002;
pub const ACCESS3_MODIFY: u32 = 0x0004;
pub const ACCESS3_EXTEND: u32 = 0x0008;
pub const ACCESS3_DELETE: u32 = 0x0010;
pub const ACCESS3_EXECUTE: u32 = 0x0020;

/// NFS status codes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum NFS3Status {
    Ok = 0,
    PermissionDenied = 1,
    NoSuchFileOrDir = 2,
    IoError = 5,
    NoSuchDevice = 6,
    AccessDenied = 13,
    FileExists = 17,
    CrossDeviceLink = 18,
    NotDir = 20,
    IsDir = 21,
    InvalidArg = 22,
    FileTooLarge = 27,
    NoSpace = 28,
    ReadOnlyFs = 30,
    TooManyLinks = 31,
    NameTooLong = 63,
    NotEmpty = 66,
    Stale = 70,
    Remote = 71,
    BadHandle = 10001,
    NotSync = 10002,
    BadCookie = 10003,
    NotSupported = 10004,
    TooSmall = 10005,
    ServerFault = 10006,
    BadType = 10007,
    Jukebox = 10008,
}

impl NFS3Status {
    pub fn from_io_error(e: &std::io::Error) -> Self {
        use std::io::ErrorKind;
        match e.kind() {
            ErrorKind::NotFound => Self::NoSuchFileOrDir,
            ErrorKind::PermissionDenied => Self::PermissionDenied,
            ErrorKind::AlreadyExists => Self::FileExists,
            ErrorKind::InvalidInput => Self::InvalidArg,
            _ => Self::IoError,
        }
    }
}

/// File type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum FileType {
    Regular = 1,
    Directory = 2,
    BlockDevice = 3,
    CharDevice = 4,
    Symlink = 5,
    Socket = 6,
    Fifo = 7,
}

/// File handle - opaque identifier for a file/directory
/// In Flint, this will encode the inode number and volume ID
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct FileHandle(pub Bytes);

impl FileHandle {
    /// Maximum file handle size (NFSv3 allows up to 64 bytes)
    pub const MAX_SIZE: usize = 64;

    /// Create a file handle from raw bytes
    pub fn new(data: Bytes) -> Self {
        assert!(data.len() <= Self::MAX_SIZE, "File handle too large");
        Self(data)
    }

    /// Create file handle from an inode number
    pub fn from_inode(inode: u64) -> Self {
        let bytes = inode.to_le_bytes();
        Self(Bytes::copy_from_slice(&bytes))
    }

    /// Get the raw bytes
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Encode to XDR
    pub fn encode(&self, enc: &mut XdrEncoder) {
        enc.encode_opaque(&self.0);
    }

    /// Decode from XDR
    pub fn decode(dec: &mut XdrDecoder) -> Result<Self, String> {
        let bytes = dec.decode_opaque()?;
        if bytes.len() > Self::MAX_SIZE {
            return Err(format!("File handle too large: {}", bytes.len()));
        }
        Ok(Self(bytes))
    }
}

/// NFS time value (seconds + nanoseconds since Unix epoch)
#[derive(Debug, Clone, Copy)]
pub struct NfsTime {
    pub seconds: u32,
    pub nanoseconds: u32,
}

impl NfsTime {
    pub fn now() -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();

        Self {
            seconds: now.as_secs() as u32,
            nanoseconds: now.subsec_nanos(),
        }
    }

    pub fn from_system_time(time: std::time::SystemTime) -> Self {
        let duration = time
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();

        Self {
            seconds: duration.as_secs() as u32,
            nanoseconds: duration.subsec_nanos(),
        }
    }

    pub fn encode(&self, enc: &mut XdrEncoder) {
        enc.encode_u32(self.seconds);
        enc.encode_u32(self.nanoseconds);
    }

    pub fn decode(dec: &mut XdrDecoder) -> Result<Self, String> {
        Ok(Self {
            seconds: dec.decode_u32()?,
            nanoseconds: dec.decode_u32()?,
        })
    }
}

/// File attributes (fattr3 in RFC 1813)
#[derive(Debug, Clone)]
pub struct FileAttr {
    pub file_type: FileType,
    pub mode: u32,         // Permission bits
    pub nlink: u32,        // Number of hard links
    pub uid: u32,          // Owner user ID
    pub gid: u32,          // Owner group ID
    pub size: u64,         // File size in bytes
    pub used: u64,         // Disk space used in bytes
    pub rdev: (u32, u32),  // Device ID (major, minor)
    pub fsid: u64,         // Filesystem ID
    pub fileid: u64,       // File ID (inode number)
    pub atime: NfsTime,    // Access time
    pub mtime: NfsTime,    // Modification time
    pub ctime: NfsTime,    // Change time
}

impl FileAttr {
    /// Create attributes from a std::fs::Metadata
    pub fn from_metadata(metadata: &std::fs::Metadata, fileid: u64) -> Self {
        use std::os::unix::fs::MetadataExt;

        let file_type = if metadata.is_dir() {
            FileType::Directory
        } else if metadata.is_symlink() {
            FileType::Symlink
        } else {
            FileType::Regular
        };

        Self {
            file_type,
            mode: metadata.mode(),
            nlink: metadata.nlink() as u32,
            uid: metadata.uid(),
            gid: metadata.gid(),
            size: metadata.len(),
            used: metadata.blocks() * 512, // blocks are 512 bytes
            rdev: (0, 0), // Not used for regular files
            fsid: 0, // Filesystem ID (can be derived from dev)
            fileid,
            atime: NfsTime::from_system_time(
                metadata.accessed().unwrap_or(std::time::SystemTime::UNIX_EPOCH)
            ),
            mtime: NfsTime::from_system_time(
                metadata.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH)
            ),
            ctime: NfsTime::from_system_time(
                metadata.created().unwrap_or(std::time::SystemTime::UNIX_EPOCH)
            ),
        }
    }

    pub fn encode(&self, enc: &mut XdrEncoder) {
        enc.encode_u32(self.file_type as u32);
        enc.encode_u32(self.mode);
        enc.encode_u32(self.nlink);
        enc.encode_u32(self.uid);
        enc.encode_u32(self.gid);
        enc.encode_u64(self.size);
        enc.encode_u64(self.used);
        enc.encode_u32(self.rdev.0);
        enc.encode_u32(self.rdev.1);
        enc.encode_u64(self.fsid);
        enc.encode_u64(self.fileid);
        self.atime.encode(enc);
        self.mtime.encode(enc);
        self.ctime.encode(enc);
    }

    pub fn decode(dec: &mut XdrDecoder) -> Result<Self, String> {
        let file_type_val = dec.decode_u32()?;
        let file_type = match file_type_val {
            1 => FileType::Regular,
            2 => FileType::Directory,
            3 => FileType::BlockDevice,
            4 => FileType::CharDevice,
            5 => FileType::Symlink,
            6 => FileType::Socket,
            7 => FileType::Fifo,
            _ => return Err(format!("Invalid file type: {}", file_type_val)),
        };

        Ok(Self {
            file_type,
            mode: dec.decode_u32()?,
            nlink: dec.decode_u32()?,
            uid: dec.decode_u32()?,
            gid: dec.decode_u32()?,
            size: dec.decode_u64()?,
            used: dec.decode_u64()?,
            rdev: (dec.decode_u32()?, dec.decode_u32()?),
            fsid: dec.decode_u64()?,
            fileid: dec.decode_u64()?,
            atime: NfsTime::decode(dec)?,
            mtime: NfsTime::decode(dec)?,
            ctime: NfsTime::decode(dec)?,
        })
    }
}

/// Filesystem statistics (fsstat3 in RFC 1813)
#[derive(Debug, Clone)]
pub struct FsStat {
    pub tbytes: u64,  // Total bytes
    pub fbytes: u64,  // Free bytes
    pub abytes: u64,  // Available bytes to non-root
    pub tfiles: u64,  // Total file slots
    pub ffiles: u64,  // Free file slots
    pub afiles: u64,  // Available file slots
    pub invarsec: u32, // Invariant in seconds
}

impl FsStat {
    pub fn encode(&self, enc: &mut XdrEncoder) {
        enc.encode_u64(self.tbytes);
        enc.encode_u64(self.fbytes);
        enc.encode_u64(self.abytes);
        enc.encode_u64(self.tfiles);
        enc.encode_u64(self.ffiles);
        enc.encode_u64(self.afiles);
        enc.encode_u32(self.invarsec);
    }
}

/// Filesystem information (fsinfo3 in RFC 1813)
#[derive(Debug, Clone)]
pub struct FsInfo {
    pub rtmax: u32,    // Max read transfer size
    pub rtpref: u32,   // Preferred read transfer size
    pub rtmult: u32,   // Suggested read multiple
    pub wtmax: u32,    // Max write transfer size
    pub wtpref: u32,   // Preferred write transfer size
    pub wtmult: u32,   // Suggested write multiple
    pub dtpref: u32,   // Preferred readdir transfer size
    pub maxfilesize: u64, // Max file size
    pub time_delta: NfsTime, // Server time granularity
    pub properties: u32, // Filesystem properties bitmask
}

impl FsInfo {
    pub fn default_config() -> Self {
        Self {
            rtmax: 1024 * 1024,     // 1 MB max read
            rtpref: 1024 * 1024,    // 1 MB preferred read
            rtmult: 4096,           // 4 KB multiple
            wtmax: 1024 * 1024,     // 1 MB max write
            wtpref: 1024 * 1024,    // 1 MB preferred write
            wtmult: 4096,           // 4 KB multiple
            dtpref: 8192,           // 8 KB readdir
            maxfilesize: u64::MAX,  // No practical limit
            time_delta: NfsTime { seconds: 0, nanoseconds: 1 }, // 1 ns granularity
            properties: 0x0008,     // FSF3_HOMOGENEOUS
        }
    }

    pub fn encode(&self, enc: &mut XdrEncoder) {
        enc.encode_u32(self.rtmax);
        enc.encode_u32(self.rtpref);
        enc.encode_u32(self.rtmult);
        enc.encode_u32(self.wtmax);
        enc.encode_u32(self.wtpref);
        enc.encode_u32(self.wtmult);
        enc.encode_u32(self.dtpref);
        enc.encode_u64(self.maxfilesize);
        self.time_delta.encode(enc);
        enc.encode_u32(self.properties);
    }
}
