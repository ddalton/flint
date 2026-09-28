//! The proxy's pseudo-root (nfs-proxy design §3): `/` lists one entry
//! per workspace THIS connection may use, and the proxy answers the ops
//! the router marks `Local` itself. It does not advertise
//! `xattr_support`, matching the hubs.
//!
//! READDIR here is written for thousands of entries, unlike the hub's
//! one-export pseudo-root: cookies are a stable hash of the NAME, so a
//! workspace created or deleted between two pages neither repeats nor
//! skips a neighbour, and a truncated page says `eof = false`.

use std::path::PathBuf;
use std::time::{Duration, UNIX_EPOCH};

use bytes::{BufMut, Bytes, BytesMut};

use super::route::{Namespace, PSEUDO_ROOT_FH};
use crate::nfs::v4::compound::{DirEntry, Operation, OperationResult, ReadDirResult};
use crate::nfs::v4::operations::fileops::{encode_attributes_from_snapshot, AttributeSnapshot};
use crate::nfs::v4::protocol::{Nfs4FileHandle, Nfs4Status};

/// What the root lists, as seen by one connection (already filtered by
/// its allowlist, like every [`Namespace`] answer).
pub trait RootListing: Namespace {
    fn workspaces(&self) -> Vec<String>;
    /// Unix seconds of the last change to the listing: the root's
    /// `change` and `mtime`, so a client's cached `/` is invalidated
    /// exactly when a workspace comes or goes.
    fn generation(&self) -> u64;
}

/// Where the current (or saved) filehandle is, for ops the proxy answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalFh {
    None,
    Pseudo,
    /// Positioned on a hub's root without the hub having been asked
    /// (a crossing or `PUTFH` of its root, then `LOOKUPP` back out).
    HubRoot,
}

#[derive(Debug, Clone)]
pub struct LocalCtx {
    pub cur: LocalFh,
    pub saved: LocalFh,
}

impl Default for LocalCtx {
    fn default() -> Self {
        LocalCtx { cur: LocalFh::None, saved: LocalFh::None }
    }
}

const ACCESS_READ: u32 = 0x01;
const ACCESS_LOOKUP: u32 = 0x02;
const ACCESS_EXECUTE: u32 = 0x20;

/// FNV-1a: stable across Rust releases (`DefaultHasher` is not
/// promised to be), and cookies must survive a proxy upgrade.
fn name_cookie(name: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in name.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    // 0 starts a listing; 1 and 2 are reserved (RFC 8881 §18.23.3).
    if h < 3 { h + 3 } else { h }
}

fn snapshot(fileid: u64, links: u32, generation: u64) -> AttributeSnapshot {
    let t = UNIX_EPOCH + Duration::from_secs(generation);
    AttributeSnapshot {
        ftype: 2, // NF4DIR
        size: 4096,
        space_used: 4096,
        fileid,
        // (0, 0): the pseudo-filesystem, distinct from every hub's
        // (server_id, 0) under H1.
        fsid_major: 0,
        fsid_minor: 0,
        atime: t,
        mtime: t,
        ctime: t,
        change: generation,
        mode: 0o755,
        numlinks: links,
        owner: 0,
        group: 0,
        path: PathBuf::from("/"),
    }
}

/// `fattr4` as `OperationResult::GetAttr` and a READDIR entry carry it:
/// the bitmap, then the values as an opaque.
fn fattr(requested: &[u32], snap: &AttributeSnapshot) -> Bytes {
    let (vals, bitmap) = encode_attributes_from_snapshot(requested, snap, false, None, None, None);
    let mut b = BytesMut::new();
    b.put_u32(bitmap.len() as u32);
    for w in &bitmap {
        b.put_u32(*w);
    }
    b.put_u32(vals.len() as u32);
    b.put_slice(&vals);
    b.put_bytes(0, (4 - vals.len() % 4) % 4);
    b.freeze()
}

fn root_attrs<L: RootListing>(ns: &L, requested: &[u32]) -> Bytes {
    let n = ns.workspaces().len() as u32;
    fattr(requested, &snapshot(1, 2 + n, ns.generation()))
}

fn xdr_len(n: usize) -> usize {
    4 + n.div_ceil(4) * 4
}

fn readdir<L: RootListing>(ns: &L, cookie: u64, dircount: u32, maxcount: u32, requested: &[u32]) -> OperationResult {
    let generation = ns.generation();
    let mut names: Vec<(u64, String)> = ns.workspaces().into_iter().map(|n| (name_cookie(&n), n)).collect();
    names.sort();
    // opcode + status + cookieverf, and the list's closing
    // `value_follows = FALSE` + `eof`.
    let mut used = 4 + 4 + 8 + 4 + 4;
    let mut dir_used = 0usize;
    let mut entries = Vec::new();
    let mut eof = true;
    for (c, name) in names.into_iter().filter(|(c, _)| *c > cookie) {
        let attrs = fattr(requested, &snapshot(c, 2, generation));
        let dir_bytes = 8 + xdr_len(name.len());
        let size = 4 + dir_bytes + attrs.len();
        let over_max = used + size > maxcount as usize;
        let over_dir = dircount > 0 && dir_used + dir_bytes > dircount as usize;
        if over_max || (over_dir && !entries.is_empty()) {
            eof = false;
            break;
        }
        used += size;
        dir_used += dir_bytes;
        entries.push(DirEntry { cookie: c, name, attrs });
    }
    if entries.is_empty() && !eof {
        // Not even one entry fits (RFC 8881 §18.23.3).
        return OperationResult::ReadDir(Nfs4Status::TooSmall, None);
    }
    OperationResult::ReadDir(Nfs4Status::Ok, Some(ReadDirResult { entries, eof, cookieverf: 1 }))
}

/// Answer one op the router marked `Local`. `opcode` is the op's own
/// (the first word of its bytes), for the error result of an op the
/// root does not support.
pub fn answer<L: RootListing>(ns: &L, ctx: &mut LocalCtx, op: &Operation, opcode: u32) -> OperationResult {
    use LocalFh::*;
    let no_fh = |ctx: &LocalCtx| ctx.cur == None;
    match op {
        Operation::PutRootFh => {
            ctx.cur = Pseudo;
            OperationResult::PutRootFh(Nfs4Status::Ok)
        }
        Operation::PutPubFh => {
            ctx.cur = Pseudo;
            OperationResult::PutPubFh(Nfs4Status::Ok)
        }
        Operation::PutFh(fh) => {
            // The router sends only the pseudo-root, or a hub root it
            // already checked, here.
            ctx.cur = if fh.data == PSEUDO_ROOT_FH { Pseudo } else { HubRoot };
            OperationResult::PutFh(Nfs4Status::Ok)
        }
        Operation::Lookup(_) => {
            // A crossing the router turned local (LOOKUPP straight back).
            ctx.cur = HubRoot;
            OperationResult::Lookup(Nfs4Status::Ok)
        }
        Operation::LookupP => {
            // Only at a hub root: back to `/`. (At `/` the router refused.)
            ctx.cur = Pseudo;
            OperationResult::LookupP(Nfs4Status::Ok)
        }
        Operation::SaveFh => {
            if no_fh(ctx) {
                return OperationResult::SaveFh(Nfs4Status::NoFileHandle);
            }
            ctx.saved = ctx.cur.clone();
            OperationResult::SaveFh(Nfs4Status::Ok)
        }
        Operation::RestoreFh => {
            if ctx.saved == None {
                return OperationResult::RestoreFh(Nfs4Status::RestoReFh);
            }
            ctx.cur = ctx.saved.clone();
            OperationResult::RestoreFh(Nfs4Status::Ok)
        }
        Operation::TestStateId(ids) => {
            // No stateid names a hub this connection may use.
            OperationResult::TestStateId(Nfs4Status::Ok, Some(vec![Nfs4Status::BadStateId; ids.len()]))
        }
        Operation::DelegPurge { .. } => OperationResult::DelegPurge(Nfs4Status::NotSupp),
        Operation::LayoutReturn { .. } => OperationResult::LayoutReturn(Nfs4Status::Ok),
        _ if no_fh(ctx) => OperationResult::Unsupported { opcode, status: Nfs4Status::NoFileHandle },
        Operation::GetFh => OperationResult::GetFh(
            Nfs4Status::Ok,
            Some(Nfs4FileHandle { data: PSEUDO_ROOT_FH.to_vec() }),
        ),
        Operation::GetAttr(req) => OperationResult::GetAttr(Nfs4Status::Ok, Some(root_attrs(ns, req))),
        Operation::Access(req) => {
            let granted = req & (ACCESS_READ | ACCESS_LOOKUP | ACCESS_EXECUTE);
            OperationResult::Access(Nfs4Status::Ok, Some((*req, granted)))
        }
        Operation::ReadDir { cookie, dircount, maxcount, attr_request, .. } => {
            readdir(ns, *cookie, *dircount, *maxcount, attr_request)
        }
        Operation::SecInfoNoName(_) => {
            // RFC 8881 §2.6.3.1.1.8: the current FH is consumed.
            ctx.cur = None;
            OperationResult::SecInfoNoName(Nfs4Status::Ok)
        }
        Operation::SecInfo(name) => {
            ctx.cur = None;
            let status = if ns.hub_by_name(name).is_some() { Nfs4Status::Ok } else { Nfs4Status::NoEnt };
            OperationResult::SecInfo(status)
        }
        Operation::SetAttr { .. } => OperationResult::SetAttr(Nfs4Status::RoFs, vec![]),
        Operation::Open { .. }
        | Operation::Create { .. }
        | Operation::Remove(_)
        | Operation::Rename { .. }
        | Operation::Link(_) => OperationResult::Unsupported { opcode, status: Nfs4Status::RoFs },
        Operation::Read { .. } | Operation::Write { .. } | Operation::ReadPlus { .. } => {
            OperationResult::Unsupported { opcode, status: Nfs4Status::IsDir }
        }
        Operation::BadXdr(_) => OperationResult::Unsupported { opcode, status: Nfs4Status::BadXdr },
        Operation::InvalidName(_) => OperationResult::Unsupported { opcode, status: Nfs4Status::Inval },
        // Unsupported ops fall here too; the encoder turns an opcode out
        // of range into OP_ILLEGAL.
        _ => OperationResult::Unsupported { opcode, status: Nfs4Status::NotSupp },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nfs::v4::compound::{CompoundResponse, CompoundRequest};
    use crate::nfs::xdr::{XdrDecoder, XdrEncoder};
    use std::cell::RefCell;

    struct Ns {
        names: RefCell<Vec<String>>,
    }
    impl Namespace for Ns {
        fn hub_by_name(&self, name: &str) -> Option<u64> {
            self.names.borrow().iter().any(|n| n == name).then_some(1)
        }
        fn hub_allowed(&self, _: u64) -> bool {
            true
        }
        fn hub_by_tag(&self, _: u32) -> Option<u64> {
            None
        }
        fn is_hub_root(&self, _: u64, _: &Nfs4FileHandle) -> bool {
            false
        }
    }
    impl RootListing for Ns {
        fn workspaces(&self) -> Vec<String> {
            self.names.borrow().clone()
        }
        fn generation(&self) -> u64 {
            1_790_000_000
        }
    }

    fn ns(n: usize) -> Ns {
        Ns { names: RefCell::new((0..n).map(|i| format!("ws-{i:05}")).collect()) }
    }

    fn page(ns: &Ns, cookie: u64, maxcount: u32) -> (Vec<String>, bool, u64) {
        let mut ctx = LocalCtx { cur: LocalFh::Pseudo, saved: LocalFh::None };
        let op = Operation::ReadDir { cookie, cookieverf: [0; 8], dircount: 0, maxcount, attr_request: vec![0x0010_0002, 0] };
        match answer(ns, &mut ctx, &op, 26) {
            OperationResult::ReadDir(Nfs4Status::Ok, Some(r)) => {
                let last = r.entries.last().map(|e| e.cookie).unwrap_or(cookie);
                (r.entries.into_iter().map(|e| e.name).collect(), r.eof, last)
            }
            x => panic!("{x:?}"),
        }
    }

    #[test]
    fn a_large_root_pages_to_the_end_and_a_truncated_page_is_not_eof() {
        let ns = ns(3000);
        let (mut seen, mut cookie, mut pages) = (Vec::new(), 0, 0);
        loop {
            let (names, eof, last) = page(&ns, cookie, 8192);
            pages += 1;
            assert!(!names.is_empty());
            seen.extend(names);
            cookie = last;
            if eof {
                break;
            }
        }
        assert!(pages > 10, "{pages} pages: the listing was not paged");
        seen.sort();
        let mut want = ns.workspaces();
        want.sort();
        assert_eq!(seen, want);
    }

    /// List page 1, apply `change`, list the rest: every name that
    /// existed throughout must appear exactly once.
    fn listing_across(change: impl Fn(&Ns, &[String])) {
        let ns = ns(200);
        let (first, eof, cookie) = page(&ns, 0, 4096);
        assert!(!eof);
        change(&ns, &first);
        let before: Vec<String> = ns.workspaces();
        let mut all = first.clone();
        let mut c = cookie;
        loop {
            let (names, eof, last) = page(&ns, c, 4096);
            all.extend(names);
            c = last;
            if eof {
                break;
            }
        }
        // a name created mid-listing may or may not appear: POSIX readdir
        for n in before.iter().filter(|n| !n.starts_with("new-")) {
            assert_eq!(all.iter().filter(|x| *x == n).count(), 1, "{n}");
        }
    }

    // Each change alone: together, an insert before the cursor and a
    // delete before it cancel out under positional cookies (measured: the
    // combined test passed with positional cookies).
    #[test]
    fn a_workspace_deleted_before_the_cursor_does_not_skip_a_neighbour() {
        listing_across(|ns, first| ns.names.borrow_mut().retain(|n| n != &first[0]));
    }

    #[test]
    fn a_workspace_created_before_the_cursor_does_not_repeat_a_neighbour() {
        listing_across(|ns, first| {
            // a name whose cookie sorts before the cursor
            let last = name_cookie(first.last().unwrap());
            let early = (0..).map(|i| format!("new-{i}")).find(|n| name_cookie(n) < last).unwrap();
            ns.names.borrow_mut().push(early);
        });
    }

    #[test]
    fn readdir_too_small_for_one_entry_says_so() {
        let ns = ns(3);
        let mut ctx = LocalCtx { cur: LocalFh::Pseudo, saved: LocalFh::None };
        let op = Operation::ReadDir { cookie: 0, cookieverf: [0; 8], dircount: 0, maxcount: 30, attr_request: vec![] };
        assert!(matches!(answer(&ns, &mut ctx, &op, 26), OperationResult::ReadDir(Nfs4Status::TooSmall, None)));
    }

    /// What a Linux mount sends at `/`, answered and encoded: the reply
    /// must be a well-formed COMPOUND4res the hub's encoder agrees with.
    #[test]
    fn the_mounts_root_probe_is_answered_without_a_hub() {
        let ns = ns(2);
        let mut ctx = LocalCtx::default();
        let ops = [
            (Operation::PutRootFh, 24),
            (Operation::GetFh, 10),
            (Operation::GetAttr(vec![0x0010_011a, 0x00b0_a23a]), 9),
            (Operation::Access(0x3f), 3),
        ];
        let mut results = Vec::new();
        for (op, code) in &ops {
            results.push(answer(&ns, &mut ctx, op, *code));
        }
        assert!(matches!(&results[1], OperationResult::GetFh(Nfs4Status::Ok, Some(fh)) if fh.data == PSEUDO_ROOT_FH));
        assert!(matches!(results[3], OperationResult::Access(Nfs4Status::Ok, Some((0x3f, 0x23)))));
        let body = CompoundResponse { status: Nfs4Status::Ok, tag: String::new(), results, raw_reply: None, cache_slot: None }.encode();
        assert!(body.len() > 40);
    }

    #[test]
    fn the_root_is_read_only_and_ops_without_a_filehandle_say_so() {
        let ns = ns(1);
        let mut ctx = LocalCtx::default();
        let r = answer(&ns, &mut ctx, &Operation::GetAttr(vec![1]), 9);
        assert_eq!(r.status(), Nfs4Status::NoFileHandle);
        ctx.cur = LocalFh::Pseudo;
        let r = answer(&ns, &mut ctx, &Operation::Remove("ws-00000".into()), 28);
        assert_eq!(r.status(), Nfs4Status::RoFs);
        let r = answer(&ns, &mut ctx, &Operation::SecInfo("ws-00000".into()), 33);
        assert_eq!((r.status(), ctx.cur.clone()), (Nfs4Status::Ok, LocalFh::None));
    }

    /// The decoder the proxy routes with reads the Linux root probe; a
    /// sanity link between the two halves.
    #[test]
    fn a_linux_style_root_probe_decodes() {
        let mut e = XdrEncoder::new();
        e.encode_opaque(b"");
        e.encode_u32(2);
        e.encode_u32(2);
        e.encode_u32(24);
        e.encode_u32(10);
        let req = CompoundRequest::decode(XdrDecoder::new(e.finish())).unwrap();
        assert_eq!(req.operations.len(), 2);
    }
}
