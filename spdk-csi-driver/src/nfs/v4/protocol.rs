// NFSv4.2 Protocol Definitions (RFC 7530, RFC 8881, RFC 7862)

/// NFSv4 RPC Program Number
pub const NFS4_PROGRAM: u32 = 100003;

/// NFSv4 Minor Versions
pub const NFS_V4_MINOR_VERSION_0: u32 = 0;  // NFSv4.0 (RFC 7530)
pub const NFS_V4_MINOR_VERSION_1: u32 = 1;  // NFSv4.1 (RFC 8881)
pub const NFS_V4_MINOR_VERSION_2: u32 = 2;  // NFSv4.2 (RFC 7862)

/// NFSv4 Procedure Numbers
/// NFSv4 has only two procedures: NULL and COMPOUND
pub mod procedure {
    pub const NULL: u32 = 0;
    pub const COMPOUND: u32 = 1;
}

/// NFSv4 Operation Codes (for COMPOUND operations)
pub mod opcode {
    // NFSv4.0 operations (RFC 7530)
    pub const ACCESS: u32 = 3;
    pub const CLOSE: u32 = 4;
    pub const COMMIT: u32 = 5;
    pub const CREATE: u32 = 6;
    pub const DELEGPURGE: u32 = 7;
    pub const DELEGRETURN: u32 = 8;
    pub const GETATTR: u32 = 9;
    pub const GETFH: u32 = 10;
    pub const LINK: u32 = 11;
    pub const LOCK: u32 = 12;
    pub const LOCKT: u32 = 13;
    pub const LOCKU: u32 = 14;
    pub const LOOKUP: u32 = 15;
    pub const LOOKUPP: u32 = 16;
    pub const NVERIFY: u32 = 17;
    pub const OPEN: u32 = 18;
    pub const OPENATTR: u32 = 19;
    pub const OPEN_CONFIRM: u32 = 20;  // Deprecated in v4.1
    pub const OPEN_DOWNGRADE: u32 = 21;
    pub const PUTFH: u32 = 22;
    pub const PUTPUBFH: u32 = 23;
    pub const PUTROOTFH: u32 = 24;
    pub const READ: u32 = 25;
    pub const READDIR: u32 = 26;
    pub const READLINK: u32 = 27;
    pub const REMOVE: u32 = 28;
    pub const RENAME: u32 = 29;
    pub const RENEW: u32 = 30;  // Deprecated in v4.1
    pub const RESTOREFH: u32 = 31;
    pub const SAVEFH: u32 = 32;
    pub const SECINFO: u32 = 33;
    pub const SETATTR: u32 = 34;
    pub const SETCLIENTID: u32 = 35;  // v4.0 only
    pub const SETCLIENTID_CONFIRM: u32 = 36;  // v4.0 only
    pub const VERIFY: u32 = 37;
    pub const WRITE: u32 = 38;
    pub const RELEASE_LOCKOWNER: u32 = 39;

    // NFSv4.1 operations (RFC 8881)
    pub const BACKCHANNEL_CTL: u32 = 40;
    pub const BIND_CONN_TO_SESSION: u32 = 41;
    pub const EXCHANGE_ID: u32 = 42;
    pub const CREATE_SESSION: u32 = 43;
    pub const DESTROY_SESSION: u32 = 44;
    pub const FREE_STATEID: u32 = 45;
    pub const GET_DIR_DELEGATION: u32 = 46;
    pub const GETDEVICEINFO: u32 = 47;
    pub const GETDEVICELIST: u32 = 48;
    pub const LAYOUTCOMMIT: u32 = 49;
    pub const LAYOUTGET: u32 = 50;
    pub const LAYOUTRETURN: u32 = 51;
    pub const SECINFO_NO_NAME: u32 = 52;
    pub const SEQUENCE: u32 = 53;
    pub const SET_SSV: u32 = 54;
    pub const TEST_STATEID: u32 = 55;
    pub const WANT_DELEGATION: u32 = 56;
    pub const DESTROY_CLIENTID: u32 = 57;
    pub const RECLAIM_COMPLETE: u32 = 58;

    // NFSv4.2 operations (RFC 7862) - Performance Features!
    pub const ALLOCATE: u32 = 59;        // Pre-allocate space
    pub const COPY: u32 = 60;            // Server-side copy
    pub const COPY_NOTIFY: u32 = 61;     // Notify for inter-server copy
    pub const DEALLOCATE: u32 = 62;      // Punch holes/deallocate
    pub const IO_ADVISE: u32 = 63;       // I/O hints
    pub const LAYOUTERROR: u32 = 64;
    pub const LAYOUTSTATS: u32 = 65;
    pub const OFFLOAD_CANCEL: u32 = 66;
    pub const OFFLOAD_STATUS: u32 = 67;
    pub const READ_PLUS: u32 = 68;       // Enhanced READ with hole detection
    pub const SEEK: u32 = 69;            // Find data/holes
    pub const WRITE_SAME: u32 = 70;      // Write pattern
    pub const CLONE: u32 = 71;           // Atomic copy-on-write clone
}

/// EXCHANGE_ID Flags (RFC 8881 Section 18.35)
pub mod exchgid_flags {
    // Client can support moved/referred filesystems
    pub const SUPP_MOVED_REFER: u32 = 0x00000001;
    // Client can support migrated filesystems
    pub const SUPP_MOVED_MIGR: u32 = 0x00000002;
    // Client wants to bind principal to stateid
    pub const BIND_PRINC_STATEID: u32 = 0x00000100;

    // Server role flags (one of these must be set in response)
    pub const USE_NON_PNFS: u32 = 0x00010000;      // Not using pNFS
    pub const USE_PNFS_MDS: u32 = 0x00020000;      // pNFS metadata server
    pub const USE_PNFS_DS: u32 = 0x00040000;       // pNFS data server
    pub const MASK_PNFS: u32 = 0x00070000;         // Mask for pNFS role bits

    // Update confirmed record
    pub const UPD_CONFIRMED_REC_A: u32 = 0x40000000;
    // Server returning confirmed clientid
    pub const CONFIRMED_R: u32 = 0x80000000;
}

/// NFSv4 Status Codes (nfsstat4)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Nfs4Status {
    Ok = 0,
    Perm = 1,               // Operation not permitted
    NoEnt = 2,              // No such file or directory
    Io = 5,                 // I/O error
    NxIo = 6,               // No such device or address
    Access = 13,            // Permission denied
    Exist = 17,             // File exists
    XDev = 18,              // Cross-device link
    NotDir = 20,            // Not a directory
    IsDir = 21,             // Is a directory
    Inval = 22,             // Invalid argument
    FBig = 27,              // File too large
    NoSpc = 28,             // No space left on device
    RoFs = 30,              // Read-only filesystem
    MLink = 31,             // Too many links
    NameTooLong = 63,       // File name too long
    NotEmpty = 66,          // Directory not empty
    DQuot = 69,             // Disk quota exceeded
    Stale = 70,             // Stale file handle
    BadHandle = 10001,      // Illegal filehandle
    BadType = 10007,        // Invalid file type
    FhExpired = 10014,      // Volatile filehandle expired
    ShareDenied = 10015,    // Share denied
    Denied = 10016,         // Lock unavailable
    ClIdInUse = 10017,      // Client ID in use
    Resource = 10018,       // Out of resource
    Moved = 10019,          // Filesystem relocated
    NoFileHandle = 10020,   // Current filehandle not set
    MinorVersMismatch = 10021,  // Minor version mismatch
    StaleClientId = 10022,  // Stale client ID
    StaleStateId = 10023,   // Stale stateid
    OldStateId = 10024,     // Old stateid
    BadStateId = 10025,     // Bad stateid
    BadSeqId = 10026,       // Bad sequence ID
    NotSame = 10027,        // Verifiers not same
    LockRange = 10028,      // Lock range error
    SymLink = 10029,        // Symlinks not supported
    RestoReFh = 10030,      // Restore FH error
    LeaseMovied = 10031,    // Lease moved
    AttrNotsupp = 10032,    // Attribute not supported
    NoGrace = 10033,        // Not in grace period
    ReclaimBad = 10034,     // Reclaim error
    ReclaimConflict = 10035, // Reclaim conflict
    BadXdr = 10036,         // Bad XDR
    LocksHeld = 10037,      // Locks held
    OpenMode = 10038,       // Bad open mode
    BadOwner = 10039,       // Bad lock owner
    BadChar = 10040,        // Bad character in name
    BadName = 10041,        // Bad component name
    BadRange = 10042,       // Bad byte range
    LockNotsupp = 10043,    // Lock not supported
    OpIllegal = 10044,      // Illegal operation
    Deadlock = 10045,       // Deadlock detected
    FileOpen = 10046,       // File is open
    AdminRevoked = 10047,   // Lock revoked by admin
    CbPathDown = 10048,     // Callback path down

    // NFSv4.1 status codes
    BadIoMode = 10049,
    BadLayout = 10050,
    BadSessionId = 10051,
    BadSession = 10052,
    LayoutTrylater = 10053,
    LayoutUnavail = 10054,
    NoMatchingLayout = 10055,
    ReclaimTooMany = 10056,
    Unknown = 10057,
    SeqMisordered = 10058,
    SequencePos = 10059,
    ReqTooBig = 10060,
    RepTooBig = 10061,
    RepTooBigToCache = 10062,
    RetryUncachedRep = 10063,
    UnsafeCompound = 10064,
    TooManyOps = 10065,
    OpNotInSession = 10066,
    HashAlgUnsupp = 10067,
    ConnNotBoundToSession = 10068,
    ClientIdBusy = 10069,
    ProtNotsupp = 10070,
    NotOnlyOp = 10071,
    NotSupp = 10072,
    ServerScopeNomatch = 10073,
    StateProtected = 10074,
    RejectedDeleg = 10075,
    ReturnConflict = 10076,
    DelegRevoked = 10077,

    // NFSv4.2 status codes
    WrongType = 10078,
    PartnerNotsupp = 10079,
    PartnerNoAuth = 10080,
    UnionNotsupp = 10081,
    OffloadDenied = 10082,
    WrongLfs = 10083,
    BadLfs = 10084,
    BadLabelPolicy = 10085,
    OffloadNoReqs = 10086,
}

impl Nfs4Status {
    pub fn from_u32(v: u32) -> Self {
        match v {
            0 => Self::Ok,
            1 => Self::Perm,
            2 => Self::NoEnt,
            5 => Self::Io,
            13 => Self::Access,
            17 => Self::Exist,
            20 => Self::NotDir,
            21 => Self::IsDir,
            22 => Self::Inval,
            28 => Self::NoSpc,
            30 => Self::RoFs,
            70 => Self::Stale,
            10001 => Self::BadHandle,
            10020 => Self::NoFileHandle,
            10021 => Self::MinorVersMismatch,
            10023 => Self::StaleStateId,
            10025 => Self::BadStateId,
            10033 => Self::NoGrace,
            10044 => Self::OpIllegal,
            10072 => Self::NotSupp,
            _ => Self::Unknown,
        }
    }

    pub fn to_u32(self) -> u32 {
        self as u32
    }
}

/// Stateid - 128-bit identifier for state (NFSv4 core concept)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StateId {
    pub seqid: u32,         // Sequence number (incremented on state changes)
    pub other: [u8; 12],    // Opaque identifier
}

impl StateId {
    pub const ANONYMOUS: StateId = StateId {
        seqid: 0,
        other: [0; 12],
    };

    pub fn new(seqid: u32, other: [u8; 12]) -> Self {
        Self { seqid, other }
    }
}

/// File handle (opaque to client, meaningful to server)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Nfs4FileHandle {
    pub data: Vec<u8>,  // Up to 128 bytes
}

impl Nfs4FileHandle {
    pub const MAX_SIZE: usize = 128;

    pub fn new(data: Vec<u8>) -> Result<Self, &'static str> {
        if data.len() > Self::MAX_SIZE {
            return Err("File handle too large");
        }
        Ok(Self { data })
    }
}

/// Client ID (opaque identifier for client)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ClientId {
    pub verifier: u64,      // Client verifier
    pub id: Vec<u8>,        // Client ID string
}

/// Session ID (128-bit identifier for NFSv4.1 sessions)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionId(pub [u8; 16]);

/// File attributes bitmap
pub mod fattr4 {
    pub const SUPPORTED_ATTRS: u32 = 0;
    pub const TYPE: u32 = 1;
    pub const FH_EXPIRE_TYPE: u32 = 2;
    pub const CHANGE: u32 = 3;
    pub const SIZE: u32 = 4;
    pub const LINK_SUPPORT: u32 = 5;
    pub const SYMLINK_SUPPORT: u32 = 6;
    pub const NAMED_ATTR: u32 = 7;
    pub const FSID: u32 = 8;
    pub const UNIQUE_HANDLES: u32 = 9;
    pub const LEASE_TIME: u32 = 10;
    pub const RDATTR_ERROR: u32 = 11;
    pub const FILEHANDLE: u32 = 19;
    pub const ACL: u32 = 12;
    pub const ACLSUPPORT: u32 = 13;
    pub const ARCHIVE: u32 = 14;
    pub const CANSETTIME: u32 = 15;
    pub const CASE_INSENSITIVE: u32 = 16;
    pub const CASE_PRESERVING: u32 = 17;
    pub const CHOWN_RESTRICTED: u32 = 18;
    pub const FILEID: u32 = 20;
    pub const FILES_AVAIL: u32 = 21;
    pub const FILES_FREE: u32 = 22;
    pub const FILES_TOTAL: u32 = 23;
    pub const FS_LOCATIONS: u32 = 24;
    pub const HIDDEN: u32 = 25;
    pub const HOMOGENEOUS: u32 = 26;
    pub const MAXFILESIZE: u32 = 27;
    pub const MAXLINK: u32 = 28;
    pub const MAXNAME: u32 = 29;
    pub const MAXREAD: u32 = 30;
    pub const MAXWRITE: u32 = 31;
    pub const MIMETYPE: u32 = 32;
    pub const MODE: u32 = 33;
    pub const NO_TRUNC: u32 = 34;
    pub const NUMLINKS: u32 = 35;
    pub const OWNER: u32 = 36;
    pub const OWNER_GROUP: u32 = 37;
    pub const QUOTA_AVAIL_HARD: u32 = 38;
    pub const QUOTA_AVAIL_SOFT: u32 = 39;
    pub const QUOTA_USED: u32 = 40;
    pub const RAWDEV: u32 = 41;
    pub const SPACE_AVAIL: u32 = 42;
    pub const SPACE_FREE: u32 = 43;
    pub const SPACE_TOTAL: u32 = 44;
    pub const SPACE_USED: u32 = 45;
    pub const SYSTEM: u32 = 46;
    pub const TIME_ACCESS: u32 = 47;
    pub const TIME_ACCESS_SET: u32 = 48;
    pub const TIME_BACKUP: u32 = 49;
    pub const TIME_CREATE: u32 = 50;
    pub const TIME_DELTA: u32 = 51;
    pub const TIME_METADATA: u32 = 52;
    pub const TIME_MODIFY: u32 = 53;
    pub const TIME_MODIFY_SET: u32 = 54;
    pub const MOUNTED_ON_FILEID: u32 = 55;
    pub const SUPPATTR_EXCLCREAT: u32 = 75;
    
    // pNFS attributes (RFC 8881 Section 5.12 and Section 12.2.2)
    pub const FS_LAYOUT_TYPES: u32 = 82;      // Supported pNFS layout types
    pub const LAYOUT_BLKSIZE: u32 = 83;       // Layout block size
}

/// File types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Nfs4FileType {
    Regular = 1,
    Directory = 2,
    BlockDevice = 3,
    CharDevice = 4,
    Symlink = 5,
    Socket = 6,
    Fifo = 7,
    AttrDir = 8,
    NamedAttr = 9,
}

/// Access bits for ACCESS operation
pub mod access4 {
    pub const READ: u32 = 0x0001;
    pub const LOOKUP: u32 = 0x0002;
    pub const MODIFY: u32 = 0x0004;
    pub const EXTEND: u32 = 0x0008;
    pub const DELETE: u32 = 0x0010;
    pub const EXECUTE: u32 = 0x0020;
}

/// Open flags
pub mod open4_share {
    pub const ACCESS_READ: u32 = 0x00000001;
    pub const ACCESS_WRITE: u32 = 0x00000002;
    pub const ACCESS_BOTH: u32 = 0x00000003;

    pub const DENY_NONE: u32 = 0x00000000;
    pub const DENY_READ: u32 = 0x00000001;
    pub const DENY_WRITE: u32 = 0x00000002;
    pub const DENY_BOTH: u32 = 0x00000003;
}

/// Constants
pub const NFS4_FHSIZE: usize = 128;
pub const NFS4_VERIFIER_SIZE: usize = 8;
pub const NFS4_OPAQUE_LIMIT: usize = 1024;
pub const NFS4_SESSIONID_SIZE: usize = 16;

/// Default lease time (90 seconds)
pub const NFS4_LEASE_TIME: u32 = 90;
