// NFSv4 Operations
//
// This module implements all NFSv4 operations (opcodes).
//
// Operation Categories:
// 1. Session Operations (NFSv4.1) - EXCHANGE_ID, CREATE_SESSION, SEQUENCE, DESTROY_SESSION
// 2. File Handle Operations - PUTROOTFH, PUTFH, GETFH, SAVEFH, RESTOREFH
// 3. Lookup/Navigation - LOOKUP, LOOKUPP, READDIR
// 4. File Operations - OPEN, CLOSE, READ, WRITE, COMMIT
// 5. Attribute Operations - GETATTR, SETATTR
// 6. Locking Operations - LOCK, LOCKT, LOCKU
// 7. NFSv4.2 Performance Operations - COPY, CLONE, ALLOCATE, DEALLOCATE, SEEK, READ_PLUS
//
// Each operation follows the pattern:
// - Parse arguments from XDR
// - Validate state (sessions, stateids, file handles)
// - Perform operation
// - Return result with status code

pub mod session;
pub mod fileops;
pub mod ioops;
pub mod perfops;  // NFSv4.2 performance operations
pub mod lockops;  // NFSv4 locking operations

pub use session::{
    SessionOperationHandler,
    ExchangeIdOp, ExchangeIdRes,
    CreateSessionOp, CreateSessionRes,
    SequenceOp, SequenceRes,
    DestroySessionOp, DestroySessionRes,
    ChannelAttrs, ClientImplId,
};

pub use fileops::{
    FileOperationHandler,
    PutRootFhOp, PutRootFhRes,
    PutFhOp, PutFhRes,
    GetFhOp, GetFhRes,
    SaveFhOp, SaveFhRes,
    RestoreFhOp, RestoreFhRes,
    LookupOp, LookupRes,
    LookupPOp, LookupPRes,
    GetAttrOp, GetAttrRes, Fattr4,
    SetAttrOp, SetAttrRes,
    AccessOp, AccessRes,
    ReadDirOp, ReadDirRes, DirEntry,
    RenameOp, RenameRes,
    LinkOp, LinkRes,
    ReadLinkOp, ReadLinkRes,
    PutPubFhOp, PutPubFhRes,
};

pub use ioops::{
    IoOperationHandler,
    OpenOp, OpenRes, OpenHow, OpenClaim, ChangeInfo,
    CloseOp, CloseRes,
    ReadOp, ReadRes,
    WriteOp, WriteRes,
    CommitOp, CommitRes,
    OpenDelegationType,
};

pub use perfops::{
    PerfOperationHandler,
    CopyOp, CopyRes, CopyCompletion,
    CloneOp, CloneRes,
    AllocateOp, AllocateRes,
    DeallocateOp, DeallocateRes,
    SeekOp, SeekRes, SeekType,
    ReadPlusOp, ReadPlusRes, ReadPlusSegment,
    IoAdviseOp, IoAdviseRes, IoAdviseHints,
};

pub use lockops::{
    LockOperationHandler,
    LockManager,
    LockOp, LockRes, LockDenied,
    LockTOp, LockTRes,
    LockUOp, LockURes,
    LockType, LockRange, Lock,
};
