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

    // RFC 5661 §15.2 — sentinel returned in the COMPOUND result array when the
    // request contained an opcode that is not a legal NFSv4 operation
    // (reserved 0/1/2 or out of range). The status accompanying it is
    // NFS4ERR_OP_ILLEGAL.
    pub const ILLEGAL: u32 = 10044;
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
// Discriminants are the wire values from RFC 7530 §13 (NFSv4.0),
// RFC 8881 §15 (NFSv4.1) and RFC 7862 §15 (NFSv4.2). Variant names are kept
// stable across this audit so existing call sites don't need to change — only
// the numeric encoding does. Cross-referenced against pynfs's
// xdrdef/nfs4_const.py.
#[repr(u32)]
pub enum Nfs4Status {
    Ok = 0,
    Perm = 1,
    NoEnt = 2,
    Io = 5,
    NxIo = 6,
    Access = 13,
    Exist = 17,
    XDev = 18,
    NotDir = 20,
    IsDir = 21,
    Inval = 22,
    FBig = 27,
    NoSpc = 28,
    RoFs = 30,
    MLink = 31,
    NameTooLong = 63,
    NotEmpty = 66,
    DQuot = 69,
    Stale = 70,

    // NFSv4.0 status codes
    BadHandle = 10001,
    BadCookie = 10003,
    NotSupp = 10004,
    TooSmall = 10005,
    ServerFault = 10006,
    BadType = 10007,
    Delay = 10008,
    Same = 10009,
    Denied = 10010,
    Expired = 10011,
    Locked = 10012,
    Grace = 10013,
    FhExpired = 10014,
    ShareDenied = 10015,
    WrongSec = 10016,
    ClIdInUse = 10017,
    Resource = 10018,
    Moved = 10019,
    NoFileHandle = 10020,
    MinorVersMismatch = 10021,
    StaleClientId = 10022,
    StaleStateId = 10023,
    OldStateId = 10024,
    BadStateId = 10025,
    BadSeqId = 10026,
    NotSame = 10027,
    LockRange = 10028,
    SymLink = 10029,
    RestoReFh = 10030,
    LeaseMovied = 10031,    // (RFC: LEASE_MOVED — variant name retained for compat)
    AttrNotsupp = 10032,
    NoGrace = 10033,
    ReclaimBad = 10034,
    ReclaimConflict = 10035,
    BadXdr = 10036,
    LocksHeld = 10037,
    OpenMode = 10038,
    BadOwner = 10039,
    BadChar = 10040,
    BadName = 10041,
    BadRange = 10042,
    LockNotsupp = 10043,
    OpIllegal = 10044,
    Deadlock = 10045,
    FileOpen = 10046,
    AdminRevoked = 10047,
    CbPathDown = 10048,

    // NFSv4.1 status codes (RFC 8881 §15)
    BadIoMode = 10049,
    BadLayout = 10050,
    BadSessionDigest = 10051,
    BadSession = 10052,         // BADSESSION (10052)
    BadSessionId = 10053,       // (RFC: BADSLOT — variant name retained)
    CompleteAlready = 10054,
    ConnNotBoundToSession = 10055,
    DelegAlreadyWanted = 10056,
    BackChanBusy = 10057,
    LayoutTrylater = 10058,
    LayoutUnavail = 10059,
    NoMatchingLayout = 10060,
    RecallConflict = 10061,
    UnknownLayoutType = 10062,
    SeqMisordered = 10063,
    SequencePos = 10064,
    ReqTooBig = 10065,
    RepTooBig = 10066,
    RepTooBigToCache = 10067,
    RetryUncachedRep = 10068,
    UnsafeCompound = 10069,
    TooManyOps = 10070,
    OpNotInSession = 10071,
    HashAlgUnsupp = 10072,
    // 10073 is unassigned per RFC 8881
    ClientIdBusy = 10074,
    PnfsIoHole = 10075,
    SeqFalseRetry = 10076,
    BadHighSlot = 10077,
    DeadSession = 10078,
    EncrAlgUnsupp = 10079,
    PnfsNoLayout = 10080,
    NotOnlyOp = 10081,
    WrongCred = 10082,
    WrongType = 10083,
    DirDelegUnavail = 10084,
    RejectedDeleg = 10085,
    ReturnConflict = 10086,
    DelegRevoked = 10087,

    // NFSv4.2 status codes (RFC 7862 §15)
    PartnerNotsupp = 10088,
    PartnerNoAuth = 10089,
    UnionNotsupp = 10090,
    OffloadDenied = 10091,
    WrongLfs = 10092,
    BadLabel = 10093,
    OffloadNoReqs = 10094,

    // Sentinel used when decoding an unknown/future status code from the wire.
    // Kept high to avoid collisions with any defined NFSv4 status value.
    Unknown = 0xFFFF_FFFF,

    // Aliases retained for older internal call sites — names removed in this
    // audit map to the corrected values above:
    //   * `ProtNotsupp`   → `NotSupp`
    //   * `BadLfs`        → `BadLabel`
    //   * `BadLabelPolicy`→ `BadLabel`
    //   * `StateProtected`→ no replacement (was misnamed; not used in code)
    //   * `ReclaimTooMany`→ no replacement (was misnamed)
    //   * `ServerScopeNomatch` → no replacement (was misnamed)
}

impl Nfs4Status {
    /// Map a wire status code to the enum. Falls back to `Unknown` for codes
    /// outside the union of NFSv4.0 / 4.1 / 4.2 RFCs — `Unknown` is a sentinel
    /// (`0xFFFF_FFFF`) and should never round-trip back onto the wire; check
    /// for it before encoding.
    pub fn from_u32(v: u32) -> Self {
        match v {
            0 => Self::Ok,
            1 => Self::Perm,
            2 => Self::NoEnt,
            5 => Self::Io,
            6 => Self::NxIo,
            13 => Self::Access,
            17 => Self::Exist,
            18 => Self::XDev,
            20 => Self::NotDir,
            21 => Self::IsDir,
            22 => Self::Inval,
            27 => Self::FBig,
            28 => Self::NoSpc,
            30 => Self::RoFs,
            31 => Self::MLink,
            63 => Self::NameTooLong,
            66 => Self::NotEmpty,
            69 => Self::DQuot,
            70 => Self::Stale,
            10001 => Self::BadHandle,
            10003 => Self::BadCookie,
            10004 => Self::NotSupp,
            10005 => Self::TooSmall,
            10006 => Self::ServerFault,
            10007 => Self::BadType,
            10008 => Self::Delay,
            10009 => Self::Same,
            10010 => Self::Denied,
            10011 => Self::Expired,
            10012 => Self::Locked,
            10013 => Self::Grace,
            10014 => Self::FhExpired,
            10015 => Self::ShareDenied,
            10016 => Self::WrongSec,
            10017 => Self::ClIdInUse,
            10018 => Self::Resource,
            10019 => Self::Moved,
            10020 => Self::NoFileHandle,
            10021 => Self::MinorVersMismatch,
            10022 => Self::StaleClientId,
            10023 => Self::StaleStateId,
            10024 => Self::OldStateId,
            10025 => Self::BadStateId,
            10026 => Self::BadSeqId,
            10027 => Self::NotSame,
            10028 => Self::LockRange,
            10029 => Self::SymLink,
            10030 => Self::RestoReFh,
            10031 => Self::LeaseMovied,
            10032 => Self::AttrNotsupp,
            10033 => Self::NoGrace,
            10034 => Self::ReclaimBad,
            10035 => Self::ReclaimConflict,
            10036 => Self::BadXdr,
            10037 => Self::LocksHeld,
            10038 => Self::OpenMode,
            10039 => Self::BadOwner,
            10040 => Self::BadChar,
            10041 => Self::BadName,
            10042 => Self::BadRange,
            10043 => Self::LockNotsupp,
            10044 => Self::OpIllegal,
            10045 => Self::Deadlock,
            10046 => Self::FileOpen,
            10047 => Self::AdminRevoked,
            10048 => Self::CbPathDown,
            10049 => Self::BadIoMode,
            10050 => Self::BadLayout,
            10051 => Self::BadSessionDigest,
            10052 => Self::BadSession,
            10053 => Self::BadSessionId,
            10054 => Self::CompleteAlready,
            10055 => Self::ConnNotBoundToSession,
            10056 => Self::DelegAlreadyWanted,
            10057 => Self::BackChanBusy,
            10058 => Self::LayoutTrylater,
            10059 => Self::LayoutUnavail,
            10060 => Self::NoMatchingLayout,
            10061 => Self::RecallConflict,
            10062 => Self::UnknownLayoutType,
            10063 => Self::SeqMisordered,
            10064 => Self::SequencePos,
            10065 => Self::ReqTooBig,
            10066 => Self::RepTooBig,
            10067 => Self::RepTooBigToCache,
            10068 => Self::RetryUncachedRep,
            10069 => Self::UnsafeCompound,
            10070 => Self::TooManyOps,
            10071 => Self::OpNotInSession,
            10072 => Self::HashAlgUnsupp,
            10074 => Self::ClientIdBusy,
            10075 => Self::PnfsIoHole,
            10076 => Self::SeqFalseRetry,
            10077 => Self::BadHighSlot,
            10078 => Self::DeadSession,
            10079 => Self::EncrAlgUnsupp,
            10080 => Self::PnfsNoLayout,
            10081 => Self::NotOnlyOp,
            10082 => Self::WrongCred,
            10083 => Self::WrongType,
            10084 => Self::DirDelegUnavail,
            10085 => Self::RejectedDeleg,
            10086 => Self::ReturnConflict,
            10087 => Self::DelegRevoked,
            10088 => Self::PartnerNotsupp,
            10089 => Self::PartnerNoAuth,
            10090 => Self::UnionNotsupp,
            10091 => Self::OffloadDenied,
            10092 => Self::WrongLfs,
            10093 => Self::BadLabel,
            10094 => Self::OffloadNoReqs,
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
    
    // pNFS attributes (RFC 8881 Section 5.12 and Section 12.2.2)
    // NOTE: Linux kernel uses different numbering than RFC 8881!
    // These values match Linux kernel include/linux/nfs4.h
    pub const FS_LAYOUT_TYPES: u32 = 62;      // Supported pNFS layout types (word 1, bit 30)
    pub const LAYOUT_TYPES: u32 = 64;         // Per-file layout types
    pub const LAYOUT_BLKSIZE: u32 = 65;       // Layout block size (word 2, bit 1)
    
    pub const SUPPATTR_EXCLCREAT: u32 = 75;
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
