//! The hub's own NFS client — in process, over the compound dispatcher.
//!
//! ## Why this exists at all
//!
//! The project service must be able to browse and edit a volume without
//! mounting it. It cannot hold kernel mounts: pod-spec volumes are fixed
//! at creation, a runtime `mount(2)` needs privilege, and — decisively —
//! a suspended hub would put every mount holder into uninterruptible
//! sleep, which is the failure class that once required deleting a Node
//! object to recover. So the hub serves an HTTP file API instead.
//!
//! ## Why it goes through the dispatcher
//!
//! The obvious implementation reads and writes the export directory
//! directly. That was tried on paper and reviewed, and it reimplements —
//! badly — semantics this server already has:
//!
//! - **eviction.** A tiered file may be a 0-byte stub with its bytes in
//!   S3. Reading the file directly returns zeros and EOF. The NFS READ
//!   path consults the marker, triggers hydration and parks the caller
//!   with NFS4ERR_DELAY; a direct reader has to be taught all of that,
//!   and the review found the first draft silently truncating downloads
//!   under an HTTP 200.
//! - **confinement.** Path containment, the export root, and the
//!   symlink rules (see `nfs::v4::open_beneath`) live in the filehandle
//!   and open layers. A second resolver is a second place for an escape.
//! - **locking, delegations, the write gate, capture notes, the space
//!   reserve.** Every mutation the tier must see rides the NFS handlers.
//!   A direct write is a write the bucket never hears about.
//!
//! Routing through [`CompoundDispatcher::dispatch_compound`] inherits
//! all of it. The API becomes a translation layer — HTTP verb to
//! COMPOUND — and nothing else. The dispatcher is a plain async fn over
//! decoded types, so there is no socket, no XDR and no loopback mount
//! involved.
//!
//! ## Minor version 0, deliberately
//!
//! Compounds are dispatched as NFSv4.0: no SEQUENCE, no session, no slot
//! table, no replay cache. NFSv4.1's session machinery exists to make a
//! lossy transport exactly-once, and there is no transport here — a
//! function call does not retransmit. Registering an internal session
//! instead would mean holding client state that must be reaped across
//! suspend and wake, for no correctness gained. The v4.2-only ops (COPY,
//! CLONE, SEEK, READ_PLUS) are gated off at this version and none of
//! them are on this surface.
//!
//! Activity accounting comes free: `dispatch_compound` already notes
//! every compound, so a browse listing lands in the Browse class and a
//! download in Data, without this module knowing the idle policy exists.

use crate::nfs::v4::compound::{
    CompoundRequest, DirEntry, Operation, OpenClaim, OpenHow, OperationResult,
};
use crate::nfs::v4::protocol::{Nfs4FileType, Nfs4Status, StateId};
use crate::nfs::v4::CompoundDispatcher;
use bytes::{BufMut, Bytes, BytesMut};
use std::sync::Arc;

/// Share bits for the API's own opens. BOTH because an upload reads back
/// nothing but the fd is cached for the write stream; DENY_NONE because
/// this caller must never fence a real client out of its own volume.
const SHARE_ACCESS_BOTH: u32 = 0x0000_0003;
const SHARE_ACCESS_READ: u32 = 0x0000_0001;
const SHARE_DENY_NONE: u32 = 0x0000_0000;

/// createmode4 / open_claim_type4 wire values (RFC 8881 §18.16.1).
const CREATE_UNCHECKED4: u32 = 0;
const CLAIM_NULL: u32 = 0;

/// Attributes requested for every listing entry, in bit order — the
/// decode below walks them in exactly this sequence, which is what the
/// fattr4 encoding guarantees.
const FATTR4_TYPE: u32 = 1;
/// The server's per-file mutation counter (`change_counter.rs`) — the
/// same value a mounted client uses to order its cache, and the one
/// this API publishes as an HTTP entity-tag. One validator for both
/// doors: a UI holding an `ETag` and a mounted process holding a
/// change value are talking about one version of one file.
const FATTR4_CHANGE: u32 = 3;
const FATTR4_SIZE: u32 = 4;
const FATTR4_FILEID: u32 = 20;
const FATTR4_MODE: u32 = 33;
const FATTR4_TIME_MODIFY: u32 = 53;

/// A path inside the export, already split and validated.
///
/// Constructed only by [`FsPath::parse`], which is the single place that
/// turns client text into components. Every component passes the
/// server's own `validate_component_name`, so `..`, embedded separators
/// and NUL never reach the dispatcher — and the dispatcher's containment
/// check remains the backstop rather than the only guard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsPath {
    components: Vec<String>,
}

impl FsPath {
    pub fn parse(raw: &str) -> Result<Self, FsError> {
        let mut components = Vec::new();
        for part in raw.split('/') {
            if part.is_empty() {
                // Leading, trailing and doubled separators are ordinary
                // sloppiness in a URL, not an attack; "" is also how the
                // root arrives.
                continue;
            }
            if let Some(status) =
                crate::nfs::v4::operations::fileops::validate_component_name(part)
            {
                return Err(FsError::Nfs(status));
            }
            components.push(part.to_string());
        }
        Ok(Self { components })
    }

    pub fn is_root(&self) -> bool {
        self.components.is_empty()
    }

    pub fn components(&self) -> &[String] {
        &self.components
    }

    /// Split into (parent, leaf). `None` at the root, which has no leaf
    /// and therefore cannot be created, removed or renamed.
    pub fn split_leaf(&self) -> Option<(FsPath, &str)> {
        let (leaf, parent) = self.components.split_last()?;
        Some((FsPath { components: parent.to_vec() }, leaf.as_str()))
    }

    /// Append one component. Validated exactly as [`FsPath::parse`]
    /// does, so a server-constructed name (the upload temp) cannot
    /// smuggle in something a client-supplied one could not.
    pub fn push_component(&mut self, name: String) {
        debug_assert!(
            crate::nfs::v4::operations::fileops::validate_component_name(&name).is_none(),
            "push_component given a name the protocol would refuse: {name:?}"
        );
        self.components.push(name);
    }

    /// `PUTROOTFH` followed by one LOOKUP per component — how any NFS
    /// client resolves a path, and the reason this API cannot resolve
    /// one the server would refuse.
    fn resolve_ops(&self) -> Vec<Operation> {
        let mut ops = Vec::with_capacity(self.components.len() + 1);
        ops.push(Operation::PutRootFh);
        for c in &self.components {
            ops.push(Operation::Lookup(c.clone()));
        }
        ops
    }

    pub fn display(&self) -> String {
        format!("/{}", self.components.join("/"))
    }
}

/// What went wrong, in the vocabulary the HTTP layer maps from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsError {
    /// The server said no. Carries the NFS status so the HTTP layer can
    /// map NOENT to 404, NOTEMPTY to 409, DELAY to 503 and so on —
    /// rather than flattening everything to 500.
    Nfs(Nfs4Status),
    /// The operation is not addressable (e.g. renaming the root).
    Invalid(&'static str),
    /// A listing cursor that no longer refers to anything.
    StaleCursor,
}

impl FsError {
    pub fn status(&self) -> Option<Nfs4Status> {
        match self {
            FsError::Nfs(s) => Some(*s),
            _ => None,
        }
    }
}

/// One entry in a listing.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub name: String,
    pub path: String,
    /// `file` | `directory` | `symlink` | `other`.
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// The LOGICAL size. For an evicted file this is the size in the
    /// bucket, not the 0 bytes on disk — the attribute path already
    /// consults the eviction marker, and a listing that reported the
    /// stub would tell a user their data was gone.
    pub size: u64,
    pub fileid: u64,
    /// The HTTP entity-tag for this object, quotes included, ready to
    /// be copied verbatim into a later `If-Match`. Serialized on every
    /// listing entry so a UI can make a conditional write without a
    /// second round trip per file.
    pub etag: String,
    pub mode: u32,
    pub modified_unix: i64,
    /// Only meaningful for symlinks: the raw target, carried as DATA and
    /// never followed. A client that wants to resolve it does so in its
    /// own namespace, which is the only namespace where it means
    /// anything.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link_target: Option<String>,
    /// The raw fattr4 CHANGE value behind [`Entry::etag`]. Not
    /// serialized — callers compare entity-tags, and publishing the
    /// halves invites someone to compare them arithmetically, which
    /// the attribute does not promise.
    #[serde(skip)]
    pub change: u64,
}

/// Render an object's identity as an HTTP entity-tag.
///
/// One caveat the callers must be told about, because it is invisible
/// from here: the change attribute is floored by ctime, and the tier
/// rewrites the local inode on both eviction (truncate in place) and
/// hydration (pwrite into the marker inode). So on a TIERED share a
/// file going cold moves its tag with no user-visible change, and a
/// caller holding one across that boundary gets a 412 for no reason it
/// can see. It fails closed — a re-read, never a lost update — and a
/// share with no tier is exact. Documented in the operator guide's
/// front-door contract; fixing it properly means a counter-only
/// validator plus a per-boot nonce, which is more machinery than the
/// annoyance has so far justified.
///
/// Both halves matter. CHANGE alone would miss a rename-over: the name
/// is rebound to a DIFFERENT inode whose own counter starts fresh and
/// can hold any value, including the one the caller remembers. The
/// fileid pins identity, the change value pins content, and VERIFY
/// checks exactly this pair.
pub fn render_etag(fileid: u64, change: u64) -> String {
    format!("\"{fileid:x}-{change:x}\"")
}

/// Inverse of [`render_etag`]. `None` for anything this server did not
/// mint — a caller inventing entity-tags gets a 400, not a silent pass.
pub fn parse_etag(raw: &str) -> Option<(u64, u64)> {
    let inner = raw.trim().strip_prefix('"')?.strip_suffix('"')?;
    let (f, c) = inner.split_once('-')?;
    Some((u64::from_str_radix(f, 16).ok()?, u64::from_str_radix(c, 16).ok()?))
}

/// A condition evaluated INSIDE the compound that performs the
/// mutation it guards.
///
/// This is NFS's own optimistic concurrency control, not a scheme
/// invented for HTTP: VERIFY (RFC 5661 §18.30) compares a supplied
/// fattr4 against the server's view of the current filehandle and
/// answers NFS4ERR_NOT_SAME on mismatch, and a compound stops at its
/// first error — so the RENAME or REMOVE behind a failed VERIFY never
/// runs.
///
/// What it is NOT: a lock. A compound is not atomic, so another
/// writer's compound can still interleave between the VERIFY and the
/// mutation. This detects a lost update between callers that use it;
/// it does not exclude a client that has the volume mounted. The
/// exclusion primitive for that is [`crate::tier::gate`], which this
/// deliberately does not take — holding a gate across an API request
/// would let a caller stall the mount.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Precondition {
    /// `If-Match: *` — the object must exist. Carried by the LOOKUP
    /// that resolves it: a missing name fails the compound before the
    /// mutation, which is the whole condition.
    Exists,
    /// `If-Match: "<etag>"` — the object must exist AND still be the
    /// one the caller read.
    Is { fileid: u64, change: u64 },
}

impl Precondition {
    /// The ops that impose this condition, leaving the compound free to
    /// append the mutation. Resolution starts from the root, so the
    /// filehandle state it leaves behind never constrains what follows.
    fn ops(&self, path: &FsPath) -> Vec<Operation> {
        let mut ops = path.resolve_ops();
        ops.extend(self.verify_op());
        ops
    }

    /// The VERIFY alone, for callers that have already positioned the
    /// current filehandle on the object under test. `Exists` needs no
    /// operation: the LOOKUP that positioned it IS the condition.
    fn verify_op(&self) -> Option<Operation> {
        match *self {
            Precondition::Exists => None,
            Precondition::Is { fileid, change } => {
                Some(Operation::Verify { attrs: verify_attrs(fileid, change) })
            }
        }
    }
}

/// The fattr4 a VERIFY of (CHANGE, FILEID) compares against.
///
/// Wire shape is the one `handle_verify` decodes: bitmap4 as a word
/// count then the words, followed by attrlist4 as a length-prefixed
/// opaque. Values run in ASCENDING attribute number — CHANGE is 3 and
/// FILEID is 20 — because that is the order the server encodes its own
/// side in, and the comparison is bytewise.
fn verify_attrs(fileid: u64, change: u64) -> Bytes {
    let mut b = BytesMut::with_capacity(28);
    b.put_u32(1); // one bitmap word: both attributes live below 32
    b.put_u32((1 << FATTR4_CHANGE) | (1 << FATTR4_FILEID));
    b.put_u32(16); // attrlist4 length
    b.put_u64(change);
    b.put_u64(fileid);
    b.freeze()
}

/// A page of a listing.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Listing {
    pub path: String,
    pub entries: Vec<Entry>,
    /// Opaque; pass back verbatim as `cursor` for the next page. `None`
    /// on the last page.
    pub next_cursor: Option<String>,
    /// Set when a recursive walk hit its bound. The caller is told
    /// explicitly rather than being handed a short list that looks
    /// complete.
    pub truncated: bool,
}

/// The in-process client.
pub struct HubFs {
    dispatcher: Arc<CompoundDispatcher>,
    /// Ceiling on entries returned in one recursive walk.
    pub recursive_max_entries: usize,
    /// Ceiling on directory depth in a recursive walk.
    pub recursive_max_depth: usize,
}

impl HubFs {
    pub fn new(dispatcher: Arc<CompoundDispatcher>) -> Self {
        Self {
            dispatcher,
            recursive_max_entries: 50_000,
            recursive_max_depth: 32,
        }
    }

    /// Dispatch one compound and return its results, or the status of
    /// the first op that failed.
    ///
    /// NFS compounds stop at the first error, so the failing op's status
    /// is the compound's status — this reports it directly rather than
    /// making every caller dig through the result vector.
    async fn compound(&self, ops: Vec<Operation>) -> Result<Vec<OperationResult>, FsError> {
        let req = CompoundRequest {
            tag: "hub-api".to_string(),
            tag_valid: true,
            // See the module doc: v4.0, no session.
            minor_version: 0,
            operations: ops,
            wire_size: 0,
        };
        let res = self.dispatcher.dispatch_compound(req, Vec::new()).await;
        if res.status != Nfs4Status::Ok {
            return Err(FsError::Nfs(res.status));
        }
        Ok(res.results)
    }

    /// GETATTR the object at `path`.
    pub async fn stat(&self, path: &FsPath) -> Result<Entry, FsError> {
        let mut ops = path.resolve_ops();
        ops.push(Operation::GetAttr(listing_attr_mask()));
        let results = self.compound(ops).await?;
        let blob = results
            .iter()
            .rev()
            .find_map(|r| match r {
                OperationResult::GetAttr(Nfs4Status::Ok, Some(b)) => Some(b.clone()),
                _ => None,
            })
            .ok_or(FsError::Nfs(Nfs4Status::ServerFault))?;
        let name = path.components().last().cloned().unwrap_or_default();
        let mut entry = decode_entry(name, path.display(), &blob)?;
        if entry.kind == "symlink" {
            entry.link_target = self.readlink(path).await.ok();
        }
        Ok(entry)
    }

    async fn readlink(&self, path: &FsPath) -> Result<String, FsError> {
        let mut ops = path.resolve_ops();
        ops.push(Operation::ReadLink);
        let results = self.compound(ops).await?;
        results
            .iter()
            .rev()
            .find_map(|r| match r {
                OperationResult::ReadLink(Nfs4Status::Ok, Some(t)) => Some(t.clone()),
                _ => None,
            })
            .ok_or(FsError::Nfs(Nfs4Status::ServerFault))
    }

    /// One page of a directory listing.
    ///
    /// `cursor` is the NFS READDIR cookie, rendered as text. It is
    /// opaque on purpose: a numeric offset would have to re-walk the
    /// directory from the start on every page, and under concurrent
    /// mutation that silently skips or repeats entries. A cookie resumes
    /// in one step and the server is the one that decides what it means.
    pub async fn list_page(
        &self,
        path: &FsPath,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Listing, FsError> {
        let cookie = match cursor {
            Some(c) => c.parse::<u64>().map_err(|_| FsError::StaleCursor)?,
            None => 0,
        };
        let (entries, eof, last_cookie) = self.readdir_raw(path, cookie, limit).await?;
        let mut out = Vec::with_capacity(entries.len());
        for de in &entries {
            let child_path = format!(
                "{}/{}",
                path.display().trim_end_matches('/'),
                de.name
            );
            let mut e = decode_entry(de.name.clone(), child_path, &de.attrs)?;
            if e.kind == "symlink" {
                let mut cp = path.clone();
                cp.push_component(de.name.clone());
                e.link_target = self.readlink(&cp).await.ok();
            }
            out.push(e);
        }
        Ok(Listing {
            path: path.display(),
            entries: out,
            next_cursor: if eof { None } else { last_cookie.map(|c| c.to_string()) },
            truncated: false,
        })
    }

    async fn readdir_raw(
        &self,
        path: &FsPath,
        cookie: u64,
        limit: usize,
    ) -> Result<(Vec<DirEntry>, bool, Option<u64>), FsError> {
        let mut ops = path.resolve_ops();
        ops.push(Operation::ReadDir {
            cookie,
            cookieverf: [0u8; 8],
            dircount: 0,
            // The server bounds a page by encoded size, so this is the
            // knob that turns `limit` into a page. Generous per entry:
            // running short only costs another round trip, which is a
            // function call.
            maxcount: (limit.clamp(1, 10_000) * 512) as u32,
            attr_request: listing_attr_mask(),
        });
        let results = self.compound(ops).await?;
        let rd = results
            .iter()
            .rev()
            .find_map(|r| match r {
                OperationResult::ReadDir(Nfs4Status::Ok, Some(rd)) => Some(rd.clone()),
                _ => None,
            })
            .ok_or(FsError::Nfs(Nfs4Status::ServerFault))?;

        let mut entries = rd.entries;
        let mut eof = rd.eof;
        if entries.len() > limit {
            entries.truncate(limit);
            eof = false;
        }
        // "." and ".." are protocol furniture, not project files.
        entries.retain(|e| e.name != "." && e.name != "..");
        let last = entries.last().map(|e| e.cookie);
        Ok((entries, eof, last))
    }

    /// Depth-first walk, bounded in both entries and depth.
    ///
    /// A recursive listing has no NFS equivalent — it is this server
    /// issuing N READDIRs on the caller's behalf — so it is bounded and
    /// says so when it stops. An unbounded one over a large project is a
    /// denial of service the hub inflicts on itself.
    pub async fn list_recursive(&self, path: &FsPath, limit: usize) -> Result<Listing, FsError> {
        let cap = limit.min(self.recursive_max_entries);
        let mut out: Vec<Entry> = Vec::new();
        let mut truncated = false;
        let mut stack: Vec<(FsPath, usize)> = vec![(path.clone(), 0)];

        while let Some((dir, depth)) = stack.pop() {
            if out.len() >= cap {
                truncated = true;
                break;
            }
            let mut cursor: Option<String> = None;
            loop {
                let page = self.list_page(&dir, cursor.as_deref(), 1000).await?;
                for e in page.entries {
                    if out.len() >= cap {
                        truncated = true;
                        break;
                    }
                    if e.kind == "directory" {
                        if depth + 1 <= self.recursive_max_depth {
                            let mut child = dir.clone();
                            child.push_component(e.name.clone());
                            stack.push((child, depth + 1));
                        } else {
                            truncated = true;
                        }
                    }
                    out.push(e);
                }
                match page.next_cursor {
                    Some(c) if !truncated => cursor = Some(c),
                    _ => break,
                }
            }
        }

        Ok(Listing {
            path: path.display(),
            entries: out,
            next_cursor: None,
            truncated,
        })
    }

    /// Read a byte range. Returns `(bytes, eof)`.
    ///
    /// `NFS4ERR_DELAY` is passed through unchanged: it means the file is
    /// evicted and hydration has been kicked off, and the HTTP layer is
    /// the right place to decide between waiting and telling the caller
    /// to come back.
    pub async fn read_at(
        &self,
        path: &FsPath,
        offset: u64,
        count: u32,
    ) -> Result<(Bytes, bool), FsError> {
        let mut ops = path.resolve_ops();
        // The anonymous stateid: READ accepts it, so a download needs no
        // OPEN and therefore mints no share reservation that could
        // conflict with a real client's.
        ops.push(Operation::Read { stateid: StateId::ANONYMOUS, offset, count });
        let results = self.compound(ops).await?;
        results
            .iter()
            .rev()
            .find_map(|r| match r {
                OperationResult::Read(Nfs4Status::Ok, Some(rr)) => {
                    Some((rr.data.clone(), rr.eof))
                }
                _ => None,
            })
            .ok_or(FsError::Nfs(Nfs4Status::ServerFault))
    }

    /// OPEN(CREATE) a file, returning the write stateid.
    pub async fn create_open(&self, path: &FsPath) -> Result<StateId, FsError> {
        let (parent, leaf) = path.split_leaf().ok_or(FsError::Invalid("cannot open the root"))?;
        let mut ops = parent.resolve_ops();
        ops.push(Operation::Open {
            seqid: 0,
            share_access: SHARE_ACCESS_BOTH,
            share_deny: SHARE_DENY_NONE,
            owner: b"flint-hub-api".to_vec(),
            // UNCHECKED4 with an empty createattrs: create if absent,
            // open if present, and change nothing about an existing
            // file. Guarded/exclusive would refuse the second upload of
            // the same name, which is the ordinary case here.
            openhow: OpenHow {
                createmode: CREATE_UNCHECKED4,
                attrs: Some(Bytes::new()),
                attrmask: vec![],
            },
            claim: OpenClaim { claim_type: CLAIM_NULL, file: leaf.to_string() },
        });
        let results = self.compound(ops).await?;
        results
            .iter()
            .rev()
            .find_map(|r| match r {
                OperationResult::Open(Nfs4Status::Ok, Some(o)) => Some(o.stateid),
                _ => None,
            })
            .ok_or(FsError::Nfs(Nfs4Status::ServerFault))
    }

    /// OPEN an existing file for reading — used to prove existence and
    /// type before a download commits to a status code.
    pub async fn open_read(&self, path: &FsPath) -> Result<StateId, FsError> {
        let (parent, leaf) = path.split_leaf().ok_or(FsError::Invalid("cannot open the root"))?;
        let mut ops = parent.resolve_ops();
        ops.push(Operation::Open {
            seqid: 0,
            share_access: SHARE_ACCESS_READ,
            share_deny: SHARE_DENY_NONE,
            owner: b"flint-hub-api".to_vec(),
            // UNCHECKED4 with NO createattrs is how the wire spells
            // "do not create" (dispatcher.rs's conversion).
            openhow: OpenHow { createmode: CREATE_UNCHECKED4, attrs: None, attrmask: vec![] },
            claim: OpenClaim { claim_type: CLAIM_NULL, file: leaf.to_string() },
        });
        let results = self.compound(ops).await?;
        results
            .iter()
            .rev()
            .find_map(|r| match r {
                OperationResult::Open(Nfs4Status::Ok, Some(o)) => Some(o.stateid),
                _ => None,
            })
            .ok_or(FsError::Nfs(Nfs4Status::ServerFault))
    }

    /// WRITE one chunk at `offset` under an open stateid. Returns bytes
    /// accepted, which the caller must check — a short write that is
    /// reported as success is how an upload silently loses its tail.
    pub async fn write_at(
        &self,
        path: &FsPath,
        stateid: StateId,
        offset: u64,
        data: Bytes,
    ) -> Result<u32, FsError> {
        let mut ops = path.resolve_ops();
        ops.push(Operation::Write {
            stateid,
            offset,
            // UNSTABLE4; the COMMIT at the end of the upload is what
            // makes the whole file durable, in one fsync rather than one
            // per chunk.
            stable: 0,
            data,
        });
        let results = self.compound(ops).await?;
        results
            .iter()
            .rev()
            .find_map(|r| match r {
                OperationResult::Write(Nfs4Status::Ok, Some(w)) => Some(w.count),
                _ => None,
            })
            .ok_or(FsError::Nfs(Nfs4Status::ServerFault))
    }

    /// COMMIT the whole file, then CLOSE.
    pub async fn commit_and_close(
        &self,
        path: &FsPath,
        stateid: StateId,
    ) -> Result<(), FsError> {
        let mut ops = path.resolve_ops();
        // offset 0 / count 0 = "everything", RFC 8881 §18.3.3.
        ops.push(Operation::Commit { offset: 0, count: 0 });
        ops.push(Operation::Close { seqid: 0, stateid });
        self.compound(ops).await?;
        Ok(())
    }

    pub async fn mkdir(&self, path: &FsPath) -> Result<(), FsError> {
        let (parent, leaf) =
            path.split_leaf().ok_or(FsError::Invalid("the root already exists"))?;
        let mut ops = parent.resolve_ops();
        ops.push(Operation::Create {
            objtype: Nfs4FileType::Directory,
            objname: leaf.to_string(),
            linkdata: None,
            // The HTTP surface exposes no mode, so ask for none and let
            // the server apply its default. Explicitly empty rather than
            // incidentally empty: CREATE now honours createattrs, so a
            // caller-supplied mode would be applied if this API ever
            // grows one.
            createattrs: crate::nfs::v4::operations::Fattr4 {
                attrmask: Vec::new(),
                attr_vals: Vec::new(),
            },
        });
        self.compound(ops).await?;
        Ok(())
    }

    /// REMOVE — files and directories both. A non-empty directory
    /// answers NFS4ERR_NOTEMPTY, which the HTTP layer renders as 409
    /// rather than deleting a tree the caller did not ask to delete.
    pub async fn remove(&self, path: &FsPath) -> Result<(), FsError> {
        self.remove_checked(path, None).await
    }

    /// REMOVE, optionally behind a [`Precondition`] evaluated in the
    /// same compound.
    pub async fn remove_checked(
        &self,
        path: &FsPath,
        expect: Option<Precondition>,
    ) -> Result<(), FsError> {
        let (parent, leaf) =
            path.split_leaf().ok_or(FsError::Invalid("cannot remove the root"))?;
        let mut ops = match expect {
            Some(p) => p.ops(path),
            None => Vec::new(),
        };
        ops.extend(parent.resolve_ops());
        ops.push(Operation::Remove(leaf.to_string()));
        self.compound(ops).await?;
        Ok(())
    }

    /// RENAME, using the SAVEFH/CFH pair the operation is defined on:
    /// resolve the source parent, SAVEFH it, resolve the target parent,
    /// then RENAME. Moves across directories fall out of that for free.
    pub async fn rename(&self, from: &FsPath, to: &FsPath) -> Result<(), FsError> {
        self.rename_checked(from, to, None, None).await
    }

    /// RENAME, optionally behind preconditions evaluated in the same
    /// compound.
    ///
    /// Two conditions because the two callers condition different
    /// objects. An upload's swap conditions its DESTINATION — "replace
    /// the file I read, not one someone else wrote since" — while a
    /// move conditions its SOURCE, which is the object the request is
    /// addressed to. Both are ordinary `If-Match`; they differ only in
    /// which name the entity-tag came from.
    pub async fn rename_checked(
        &self,
        from: &FsPath,
        to: &FsPath,
        expect_from: Option<Precondition>,
        expect_to: Option<Precondition>,
    ) -> Result<(), FsError> {
        let (from_parent, from_leaf) =
            from.split_leaf().ok_or(FsError::Invalid("cannot rename the root"))?;
        let (to_parent, to_leaf) =
            to.split_leaf().ok_or(FsError::Invalid("cannot rename onto the root"))?;

        // Both conditions are resolved from the root at the head of the
        // compound, ahead of the mutation.
        //
        // Evaluating them LATER would be better — a COMPOUND is not
        // atomic, so every operation between the VERIFY and the RENAME
        // is a yield point where another writer can land its own rename
        // and consume the version this one just checked. Positioning
        // the VERIFY on the target and stepping back up with LOOKUPP
        // would leave one operation in that gap instead of five. It was
        // tried and it does not work: the filehandle LOOKUPP yields is
        // not one RENAME accepts here, and every conditional write
        // answered STALE. The window stays as wide as this ordering
        // makes it, which is why the contract this surface publishes is
        // detection rather than exclusion — see the drill.
        let mut ops = Vec::new();
        if let Some(p) = expect_from {
            ops.extend(p.ops(from));
        }
        if let Some(p) = expect_to {
            ops.extend(p.ops(to));
        }
        ops.extend(from_parent.resolve_ops());
        ops.push(Operation::SaveFh);
        ops.extend(to_parent.resolve_ops());
        ops.push(Operation::Rename {
            oldname: from_leaf.to_string(),
            newname: to_leaf.to_string(),
        });
        self.compound(ops).await?;
        Ok(())
    }
}

/// The attribute mask every listing requests, as bitmap words.
fn listing_attr_mask() -> Vec<u32> {
    let mut words = vec![0u32; 2];
    for a in [
        FATTR4_TYPE,
        FATTR4_CHANGE,
        FATTR4_SIZE,
        FATTR4_FILEID,
        FATTR4_MODE,
        FATTR4_TIME_MODIFY,
    ] {
        words[(a / 32) as usize] |= 1 << (a % 32);
    }
    words
}

/// Decode the fattr4 blob for one entry.
///
/// fattr4 values appear in ASCENDING ATTRIBUTE NUMBER, which is what
/// makes this safe without a general decoder: the mask is ours
/// ([`listing_attr_mask`]), so the order is fixed and known. A server
/// that could not encode one of them simply omits it, so each field is
/// read only if its bit is set in the RETURNED mask, not the requested
/// one.
fn decode_entry(name: String, path: String, blob: &Bytes) -> Result<Entry, FsError> {
    use crate::nfs::v4::xdr::AttrDecoder;

    // The blob is `bitmap4 + attrlist4` as GETATTR/READDIR encode it.
    let mut dec = AttrDecoder::new(blob.clone());
    let word_count = dec.decode_u32().map_err(|_| FsError::Nfs(Nfs4Status::BadXdr))? as usize;
    let mut mask = Vec::with_capacity(word_count);
    for _ in 0..word_count.min(8) {
        mask.push(dec.decode_u32().map_err(|_| FsError::Nfs(Nfs4Status::BadXdr))?);
    }
    // attrlist4 is opaque<>: a length then the values.
    let _len = dec.decode_u32().map_err(|_| FsError::Nfs(Nfs4Status::BadXdr))?;

    let has = |a: u32| -> bool {
        let w = (a / 32) as usize;
        w < mask.len() && mask[w] & (1 << (a % 32)) != 0
    };

    let mut kind = "other";
    let mut change = 0u64;
    let mut size = 0u64;
    let mut fileid = 0u64;
    let mut mode = 0u32;
    let mut modified_unix = 0i64;

    if has(FATTR4_TYPE) {
        let t = dec.decode_u32().map_err(|_| FsError::Nfs(Nfs4Status::BadXdr))?;
        kind = match t {
            1 => "file",
            2 => "directory",
            5 => "symlink",
            _ => "other",
        };
    }
    // Attribute 3, so it decodes after TYPE and before SIZE. The order
    // is the fattr4 encoding's, not this function's choice.
    if has(FATTR4_CHANGE) {
        change = dec.decode_u64().map_err(|_| FsError::Nfs(Nfs4Status::BadXdr))?;
    }
    if has(FATTR4_SIZE) {
        size = dec.decode_u64().map_err(|_| FsError::Nfs(Nfs4Status::BadXdr))?;
    }
    if has(FATTR4_FILEID) {
        fileid = dec.decode_u64().map_err(|_| FsError::Nfs(Nfs4Status::BadXdr))?;
    }
    if has(FATTR4_MODE) {
        mode = dec.decode_u32().map_err(|_| FsError::Nfs(Nfs4Status::BadXdr))?;
    }
    if has(FATTR4_TIME_MODIFY) {
        let (s, _ns) = dec
            .decode_nfstime4()
            .map_err(|_| FsError::Nfs(Nfs4Status::BadXdr))?;
        modified_unix = s;
    }

    Ok(Entry {
        name,
        path,
        kind,
        size,
        fileid,
        etag: render_etag(fileid, change),
        mode,
        modified_unix,
        link_target: None,
        change,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_split_and_refused_by_the_servers_own_rules() {
        assert!(FsPath::parse("/").unwrap().is_root());
        assert!(FsPath::parse("").unwrap().is_root());
        assert_eq!(
            FsPath::parse("/a/b/c.txt").unwrap().components(),
            ["a", "b", "c.txt"]
        );
        // Sloppy separators are tolerated.
        assert_eq!(FsPath::parse("//a///b/").unwrap().components(), ["a", "b"]);

        // Traversal is refused HERE, before the dispatcher, using the
        // same validator every NFS op uses — so the API cannot express
        // a name the protocol would reject.
        assert!(matches!(
            FsPath::parse("/a/../../etc/passwd"),
            Err(FsError::Nfs(Nfs4Status::BadName))
        ));
        assert!(matches!(
            FsPath::parse("/a/./b"),
            Err(FsError::Nfs(Nfs4Status::BadName))
        ));
        assert!(matches!(
            FsPath::parse("/a/b\0c"),
            Err(FsError::Nfs(Nfs4Status::BadName))
        ));
    }

    #[test]
    fn the_root_has_no_leaf_so_it_cannot_be_mutated() {
        let root = FsPath::parse("/").unwrap();
        assert!(root.split_leaf().is_none());
        let one = FsPath::parse("/only").unwrap();
        let (parent, leaf) = one.split_leaf().unwrap();
        assert!(parent.is_root());
        assert_eq!(leaf, "only");
    }

    #[test]
    fn resolution_is_putrootfh_then_one_lookup_per_component() {
        let p = FsPath::parse("/a/b").unwrap();
        let ops = p.resolve_ops();
        assert!(matches!(ops[0], Operation::PutRootFh));
        assert!(matches!(&ops[1], Operation::Lookup(n) if n == "a"));
        assert!(matches!(&ops[2], Operation::Lookup(n) if n == "b"));
        assert_eq!(ops.len(), 3);
    }

    #[test]
    fn the_listing_mask_names_exactly_the_attrs_the_decoder_walks() {
        let mask = listing_attr_mask();
        for a in [
            FATTR4_TYPE,
            FATTR4_CHANGE,
            FATTR4_SIZE,
            FATTR4_FILEID,
            FATTR4_MODE,
            FATTR4_TIME_MODIFY,
        ] {
            assert!(mask[(a / 32) as usize] & (1 << (a % 32)) != 0, "attr {a} missing");
        }
        // Nothing else, or the decoder's positional walk would desync.
        let set: u32 = mask.iter().map(|w| w.count_ones()).sum();
        assert_eq!(set, 6);
    }
}
