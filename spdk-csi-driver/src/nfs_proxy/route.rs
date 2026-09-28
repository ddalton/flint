//! The COMPOUND router (nfs-proxy design §3): which target each op of a
//! downstream compound runs on. Pure — it sees the decoded ops and a
//! [`Namespace`] view, and returns a [`Plan`]; it touches no socket.
//!
//! The routing key is the current filehandle. A compound is a proxy-only
//! PREFIX (the pseudo-root, answered here) followed by ops for at most
//! ONE hub, forwarded byte-for-byte. The crossing into a workspace is
//! `PUTFH(pseudo-root), LOOKUP name` (census s1, s4): the proxy answers
//! both, and the hub receives a `PUTROOTFH` in their place.
//!
//! The allowlist (§7, §8 step 2) is enforced HERE, at the three points
//! where a connection can reach a hub: the crossing `LOOKUP` (a refused
//! workspace is `NOENT`, indistinguishable from an absent one), `PUTFH`
//! of a hub filehandle (`STALE` — a leaked filehandle must not bypass
//! the `LOOKUP` check), and a stateid's hub tag (`BAD_STATEID`).
//! `READDIR /` is filtered by the pseudo-root, from the same view.

use crate::nfs::v4::compound::Operation;
use crate::nfs::v4::protocol::{Nfs4FileHandle, Nfs4Status, StateId};

/// The proxy's pseudo-root filehandle. Byte 0 is a version no hub mints
/// (hubs mint 1..=4, `filehandle.rs` `validate_handle`). Constant, so a
/// client's cached root survives a proxy restart.
pub const PSEUDO_ROOT_FH: &[u8] = &[0xF0, b'f', b'l', b'i', b'n', b't', b'p', b'x', b'y'];

pub fn is_pseudo_root(fh: &Nfs4FileHandle) -> bool {
    fh.data == PSEUDO_ROOT_FH
}

/// A hub filehandle carries the hub's `instance_id` (a lite hub's
/// persistent `server_id`) in the clear at bytes [1..9], for every
/// version the hub mints. `None` = not a hub handle at all.
pub fn hub_of_fh(fh: &Nfs4FileHandle) -> Option<u64> {
    let d = &fh.data;
    if d.len() < 9 || !(1..=4).contains(&d[0]) {
        return None;
    }
    Some(u64::from_be_bytes(d[1..9].try_into().unwrap()))
}

/// H2: the hub tag a stateid carries in `other[8..12]` (big-endian, as
/// `allocate` writes it).
pub fn tag_of_stateid(s: &StateId) -> u32 {
    u32::from_be_bytes(s.other[8..12].try_into().unwrap())
}

/// What the router may know about hubs, as seen by ONE connection: every
/// answer is already filtered by that connection's allowlist, so a hub
/// the connection may not use is indistinguishable from one that does
/// not exist.
pub trait Namespace {
    /// The workspace `name` under `/`, if it exists AND is allowed.
    fn hub_by_name(&self, name: &str) -> Option<u64>;
    /// A hub (by server id) this connection may use.
    fn hub_allowed(&self, server_id: u64) -> bool;
    /// The hub whose stateid tag is `tag`, if allowed.
    fn hub_by_tag(&self, tag: u32) -> Option<u64>;
    /// `fh` is the root filehandle of hub `server_id` (learned from the
    /// backend's own `PUTROOTFH, GETFH`). Only `LOOKUPP` needs it.
    fn is_hub_root(&self, server_id: u64, fh: &Nfs4FileHandle) -> bool;
}

/// How one downstream op is executed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Disp {
    /// The proxy answers it (pseudo-root ops, FH-less ops with no hub).
    Local,
    /// A session op (`DESTROY_SESSION`, `RECLAIM_COMPLETE`, ...) the
    /// proxy's own session layer answers.
    Session,
    /// The crossing `LOOKUP`: answered `NFS4_OK` by the proxy; the hub
    /// receives a `PUTROOTFH` in its place.
    Cross,
    /// Its original bytes go to the plan's hub.
    Forward,
    /// The proxy answers this status and the compound stops here.
    Refuse(Nfs4Status),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// One entry per routed op (the ops after `SEQUENCE`), ending at the
    /// first `Refuse`, if any.
    pub disp: Vec<Disp>,
    /// The hub the `Forward` ops go to. `Some` iff any op is `Forward`.
    pub hub: Option<u64>,
    /// The backend compound starts with a synthesised `PUTROOTFH`.
    pub putrootfh: bool,
    /// The refusal needed a SECOND target (§3: logged and counted; the
    /// first drill must show this at zero for the Linux client).
    pub second_target: bool,
}
// Shape: `Local|Session|Cross`* then `Forward`* then at most one
// `Local` (the foreign PUTFH before an XDEV) and one `Refuse`. Ops after
// the forwarded run are answered only if the hub ran all of its ops OK:
// a hub failure ends the compound there, as it would on a direct mount.

/// The target the current (or saved) filehandle belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum At {
    None,
    Pseudo,
    /// `root` = known to be the hub's root (after a crossing or a
    /// `PUTFH` of its root FH), which only `LOOKUPP` cares about.
    Hub { id: u64, root: bool },
}

impl At {
    fn hub(self) -> Option<u64> {
        match self {
            At::Hub { id, .. } => Some(id),
            _ => None,
        }
    }
}

/// Ops that leave the current filehandle where it is.
fn keeps_fh(op: &Operation) -> bool {
    matches!(
        op,
        Operation::GetFh
            | Operation::GetAttr(_)
            | Operation::Access(_)
            | Operation::Verify { .. }
            | Operation::Nverify { .. }
            | Operation::SaveFh
            | Operation::ReadDir { .. }
    )
}

/// Ops answered by the proxy's session layer, wherever they appear.
fn is_session_op(op: &Operation) -> bool {
    matches!(
        op,
        Operation::ExchangeId { .. }
            | Operation::CreateSession { .. }
            | Operation::DestroySession(_)
            | Operation::DestroyClientId(_)
            | Operation::BindConnToSession { .. }
            | Operation::Sequence { .. }
            | Operation::ReclaimComplete(_)
    )
}

/// Two-FH ops: the saved FH and the current FH must be on one hub.
fn uses_saved_fh(op: &Operation) -> bool {
    matches!(
        op,
        Operation::Rename { .. } | Operation::Link(_) | Operation::Copy { .. } | Operation::Clone { .. }
    )
}

struct Router<'a, N: Namespace> {
    ns: &'a N,
    disp: Vec<Disp>,
    hub: Option<u64>,
    putrootfh: bool,
    /// Ops that only POSITION onto a hub (the crossing, or a `PUTFH` of
    /// its root) are held here until something needs the hub: a
    /// `LOOKUPP` straight back to `/` then needs no hub at all.
    pending: Option<usize>,
    cur: At,
    saved: At,
}

impl<'a, N: Namespace> Router<'a, N> {
    /// Bind the compound to hub `id`. A second hub is a refusal.
    fn bind(&mut self, id: u64) -> Result<(), Nfs4Status> {
        match self.hub {
            None => {
                self.hub = Some(id);
                Ok(())
            }
            Some(h) if h == id => Ok(()),
            Some(_) => Err(Nfs4Status::ServerFault),
        }
    }

    /// Once anything is forwarded, nothing may run locally after it
    /// (the local answer would land in the middle of the hub's results).
    fn forwarding(&self) -> bool {
        self.disp.iter().any(|d| *d == Disp::Forward)
    }

    /// Commit the held positioning ops: they reach the hub.
    fn commit_pending(&mut self) {
        if let Some(i) = self.pending.take() {
            for d in &mut self.disp[i..] {
                if *d == Disp::Local {
                    // a held PUTFH of a hub root
                    *d = Disp::Forward;
                }
            }
        }
    }

    fn refuse(&mut self, status: Nfs4Status, second_target: bool) -> Plan {
        self.disp.push(Disp::Refuse(status));
        self.finish(second_target)
    }

    fn finish(&mut self, second_target: bool) -> Plan {
        // Held positioning with nothing behind it still reaches the hub:
        // the client may GETFH/GETATTR the result in a LATER compound,
        // and the crossing's LOOKUP must have been checked against the
        // hub it names (a parked hub must wake, §4).
        self.commit_pending();
        let forwarded = self.disp.iter().any(|d| matches!(d, Disp::Forward | Disp::Cross));
        Plan {
            disp: std::mem::take(&mut self.disp),
            hub: if forwarded { self.hub } else { None },
            putrootfh: self.putrootfh && forwarded,
            second_target,
        }
    }

    /// Route one op that runs against the current target.
    fn on_current(&mut self, op: &Operation) -> Result<Disp, (Nfs4Status, bool)> {
        match self.cur {
            At::None => {
                if self.forwarding() {
                    // e.g. SECINFO_NO_NAME cleared the FH mid-hub: the hub
                    // answers NOFILEHANDLE itself.
                    return Ok(Disp::Forward);
                }
                Ok(Disp::Local)
            }
            At::Pseudo => {
                if self.forwarding() || self.pending.is_some() {
                    return Err((Nfs4Status::ServerFault, true));
                }
                Ok(Disp::Local)
            }
            At::Hub { id, .. } => {
                self.bind(id).map_err(|s| (s, true))?;
                if !keeps_fh(op) {
                    self.cur = At::Hub { id, root: false };
                }
                self.commit_pending();
                Ok(Disp::Forward)
            }
        }
    }

    fn route(mut self, ops: &[Operation]) -> Plan {
        for (i, op) in ops.iter().enumerate() {
            let d = match op {
                Operation::PutRootFh | Operation::PutPubFh => {
                    if self.hub.is_some() {
                        return self.refuse(Nfs4Status::ServerFault, true);
                    }
                    self.cur = At::Pseudo;
                    Disp::Local
                }
                Operation::PutFh(fh) if is_pseudo_root(fh) => {
                    if self.hub.is_some() {
                        return self.refuse(Nfs4Status::ServerFault, true);
                    }
                    self.cur = At::Pseudo;
                    Disp::Local
                }
                Operation::PutFh(fh) => {
                    let Some(id) = hub_of_fh(fh) else {
                        // Not a handle any hub minted: BADHANDLE, as the
                        // hub's own validator would say.
                        return self.refuse(Nfs4Status::BadHandle, false);
                    };
                    if !self.ns.hub_allowed(id) {
                        // Unknown, parked-and-gone, or not this client's:
                        // one answer, so a leaked FH learns nothing.
                        return self.refuse(Nfs4Status::Stale, false);
                    }
                    if self.bind(id).is_err() {
                        // A second hub. The one shape that needs it on
                        // purpose is the cross-workspace two-FH op, `PUTFH(a)
                        // SAVEFH PUTFH(b) RENAME`: answer this PUTFH here and
                        // let the RENAME get its §3 XDEV, uncounted.
                        let two_fh_next = ops.get(i + 1).is_some_and(uses_saved_fh);
                        if two_fh_next && self.saved.hub() == self.hub {
                            self.cur = At::Hub { id, root: false };
                            self.disp.push(Disp::Local);
                            continue;
                        }
                        return self.refuse(Nfs4Status::ServerFault, true);
                    }
                    let root = self.ns.is_hub_root(id, fh);
                    self.cur = At::Hub { id, root };
                    if root && !self.forwarding() && self.pending.is_none() {
                        self.pending = Some(self.disp.len());
                        Disp::Local
                    } else {
                        self.commit_pending();
                        Disp::Forward
                    }
                }
                Operation::Lookup(name) if self.cur == At::Pseudo => {
                    let Some(id) = self.ns.hub_by_name(name) else {
                        return self.refuse(Nfs4Status::NoEnt, false);
                    };
                    if self.bind(id).is_err() {
                        return self.refuse(Nfs4Status::ServerFault, true);
                    }
                    self.putrootfh = true;
                    self.pending = Some(self.disp.len());
                    self.cur = At::Hub { id, root: true };
                    Disp::Cross
                }
                Operation::LookupP => match self.cur {
                    // RFC 8881 §18.14: LOOKUPP at the root is NOENT.
                    At::Pseudo => {
                        if self.forwarding() || self.pending.is_some() {
                            return self.refuse(Nfs4Status::ServerFault, true);
                        }
                        return self.refuse(Nfs4Status::NoEnt, false);
                    }
                    At::Hub { root: true, .. } if !self.forwarding() => {
                        // Straight back out: the positioning ops never
                        // reach the hub, and the compound is local again.
                        if let Some(i) = self.pending.take() {
                            for d in &mut self.disp[i..] {
                                if *d == Disp::Cross {
                                    // answered OK locally all the same
                                    *d = Disp::Local;
                                }
                            }
                        }
                        self.hub = None;
                        self.putrootfh = false;
                        self.cur = At::Pseudo;
                        Disp::Local
                    }
                    At::Hub { root: true, .. } => {
                        return self.refuse(Nfs4Status::ServerFault, true);
                    }
                    _ => match self.on_current(op) {
                        Ok(d) => d,
                        Err((s, second)) => return self.refuse(s, second),
                    },
                },
                Operation::SaveFh => {
                    let d = match self.on_current(op) {
                        Ok(d) => d,
                        Err((s, second)) => return self.refuse(s, second),
                    };
                    self.saved = self.cur;
                    d
                }
                Operation::RestoreFh => {
                    // The saved target travels with the saved FH.
                    let back = self.saved;
                    match (back, self.hub) {
                        (At::Hub { id, .. }, _) => {
                            if self.bind(id).is_err() {
                                return self.refuse(Nfs4Status::ServerFault, true);
                            }
                            self.cur = back;
                            self.commit_pending();
                            Disp::Forward
                        }
                        (At::Pseudo, None) => {
                            self.cur = At::Pseudo;
                            Disp::Local
                        }
                        (At::Pseudo, Some(_)) => {
                            return self.refuse(Nfs4Status::ServerFault, true);
                        }
                        (At::None, _) => match self.on_current(op) {
                            // the hub (or the proxy) answers RESTOREFH
                            // with nothing saved: NOFILEHANDLE / RESTOREFH
                            Ok(d) => d,
                            Err((s, second)) => return self.refuse(s, second),
                        },
                    }
                }
                op if uses_saved_fh(op) => {
                    let (s, c) = (self.saved.hub(), self.cur.hub());
                    if let (Some(s), Some(c)) = (s, c) {
                        if s != c {
                            // §3: a cross-workspace RENAME/LINK/COPY/CLONE.
                            // Linux refuses it locally (distinct fsids, H1);
                            // this is the backstop.
                            return self.refuse(Nfs4Status::XDev, false);
                        }
                    }
                    if s.is_some() != c.is_some() && self.saved != At::None {
                        // one side on the pseudo-root: nothing to rename
                        // across, the root is read-only
                        return self.refuse(Nfs4Status::XDev, false);
                    }
                    match self.on_current(op) {
                        Ok(d) => d,
                        Err((s, second)) => return self.refuse(s, second),
                    }
                }
                Operation::TestStateId(ids) => {
                    // H2: route by the hub tag. Every stateid must name the
                    // SAME allowed hub; if none names one, the proxy answers
                    // (each is BAD_STATEID).
                    let mut hub = None;
                    let mut mixed = false;
                    for s in ids {
                        if let Some(h) = self.ns.hub_by_tag(tag_of_stateid(s)) {
                            match hub {
                                None => hub = Some(h),
                                Some(x) if x == h => {}
                                Some(_) => mixed = true,
                            }
                        }
                    }
                    if mixed {
                        return self.refuse(Nfs4Status::ServerFault, true);
                    }
                    match hub {
                        None if !self.forwarding() && self.pending.is_none() => Disp::Local,
                        None => return self.refuse(Nfs4Status::ServerFault, true),
                        Some(h) => {
                            if self.bind(h).is_err() {
                                return self.refuse(Nfs4Status::ServerFault, true);
                            }
                            self.commit_pending();
                            Disp::Forward
                        }
                    }
                }
                Operation::FreeStateId(s) => match self.ns.hub_by_tag(tag_of_stateid(s)) {
                    // §5: fan-out is wrong, FREE_STATEID on the wrong hub
                    // frees real state.
                    None => return self.refuse(Nfs4Status::BadStateId, false),
                    Some(h) => {
                        if self.bind(h).is_err() {
                            return self.refuse(Nfs4Status::ServerFault, true);
                        }
                        self.commit_pending();
                        Disp::Forward
                    }
                },
                // §5: FH-less, and no live state in lite: the proxy answers.
                Operation::DelegPurge { .. } | Operation::LayoutReturn { .. }
                    if !self.forwarding() && self.pending.is_none() =>
                {
                    Disp::Local
                }
                op if is_session_op(op) => {
                    if self.forwarding() || self.pending.is_some() {
                        return self.refuse(Nfs4Status::ServerFault, true);
                    }
                    Disp::Session
                }
                Operation::Unsupported(_) | Operation::BadXdr(_) => {
                    // Decoding stopped here: this op's span runs to the end
                    // of the buffer and is opaque. It goes to the current
                    // target, which runs the same decoder and fails the
                    // same way; the proxy answers it on the pseudo-root.
                    let d = match self.on_current(op) {
                        Ok(d) => d,
                        Err((s, second)) => return self.refuse(s, second),
                    };
                    self.disp.push(d);
                    return self.finish(false);
                }
                op => match self.on_current(op) {
                    Ok(d) => d,
                    Err((s, second)) => return self.refuse(s, second),
                },
            };
            self.disp.push(d);
        }
        self.finish(false)
    }
}

/// Route the ops AFTER the downstream `SEQUENCE`.
pub fn route<N: Namespace>(ns: &N, ops: &[Operation]) -> Plan {
    Router {
        ns,
        disp: Vec::with_capacity(ops.len()),
        hub: None,
        putrootfh: false,
        pending: None,
        cur: At::None,
        saved: At::None,
    }
    .route(ops)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Two hubs, `ws-a` (id 0xA, tag 10) and `ws-b` (id 0xB, tag 11), and
    /// `ws-c` (id 0xC, tag 12) which exists but this connection may NOT use.
    struct Ns {
        names: HashMap<&'static str, u64>,
        allowed: Vec<u64>,
    }

    fn ns() -> Ns {
        Ns {
            names: [("ws-a", 0xA), ("ws-b", 0xB), ("ws-c", 0xC)].into_iter().collect(),
            allowed: vec![0xA, 0xB],
        }
    }

    impl Namespace for Ns {
        fn hub_by_name(&self, name: &str) -> Option<u64> {
            self.names.get(name).copied().filter(|id| self.allowed.contains(id))
        }
        fn hub_allowed(&self, id: u64) -> bool {
            self.allowed.contains(&id)
        }
        fn hub_by_tag(&self, tag: u32) -> Option<u64> {
            let id = match tag {
                10 => 0xA,
                11 => 0xB,
                12 => 0xC,
                _ => return None,
            };
            self.hub_allowed(id).then_some(id)
        }
        fn is_hub_root(&self, id: u64, fh: &Nfs4FileHandle) -> bool {
            fh.data == hub_fh(id, 0).data
        }
    }

    /// A v3-shaped hub handle for object `n` on hub `id` (`n == 0` is its root).
    fn hub_fh(id: u64, n: u8) -> Nfs4FileHandle {
        let mut d = vec![3];
        d.extend_from_slice(&id.to_be_bytes());
        d.extend_from_slice(&[n; 42]);
        Nfs4FileHandle { data: d }
    }

    fn root() -> Operation {
        Operation::PutFh(Nfs4FileHandle { data: PSEUDO_ROOT_FH.to_vec() })
    }
    fn putfh(id: u64, n: u8) -> Operation {
        Operation::PutFh(hub_fh(id, n))
    }
    fn lookup(s: &str) -> Operation {
        Operation::Lookup(s.to_string())
    }
    fn getattr() -> Operation {
        Operation::GetAttr(vec![0x10])
    }
    fn stateid(tag: u32) -> StateId {
        let mut other = [7u8; 12];
        other[8..12].copy_from_slice(&tag.to_be_bytes());
        StateId { seqid: 1, other }
    }
    fn rename() -> Operation {
        Operation::Rename { oldname: "x".into(), newname: "y".into() }
    }

    use Disp::*;

    #[test]
    fn the_crossing_is_answered_here_and_the_hub_gets_a_putrootfh() {
        // census s1/s4: the mount's submount crossing
        let p = route(&ns(), &[root(), lookup("ws-a"), Operation::GetFh, getattr()]);
        assert_eq!(p.disp, vec![Local, Cross, Forward, Forward]);
        assert_eq!(p.hub, Some(0xA));
        assert!(p.putrootfh);
        assert!(!p.second_target);
    }

    #[test]
    fn a_bare_crossing_still_reaches_the_hub() {
        // PUTFH(/) LOOKUP ws with nothing behind it: the LOOKUP must have
        // been answered by the hub it names (a parked hub wakes, §4).
        let p = route(&ns(), &[root(), lookup("ws-a")]);
        assert_eq!(p.disp, vec![Local, Cross]);
        assert_eq!(p.hub, Some(0xA));
        assert!(p.putrootfh);
    }

    #[test]
    fn the_pseudo_root_alone_is_local() {
        let p = route(&ns(), &[Operation::PutRootFh, Operation::GetFh, getattr()]);
        assert_eq!(p.disp, vec![Local, Local, Local]);
        assert_eq!(p.hub, None);
        assert!(!p.putrootfh);
    }

    #[test]
    fn a_workspace_this_connection_may_not_use_looks_absent() {
        // The allowlist's first point: the crossing LOOKUP. Control: the
        // same LOOKUP of an allowed workspace crosses.
        let refused = route(&ns(), &[root(), lookup("ws-c"), getattr()]);
        let absent = route(&ns(), &[root(), lookup("nope"), getattr()]);
        assert_eq!(refused, absent);
        assert_eq!(refused.disp, vec![Local, Refuse(Nfs4Status::NoEnt)]);
        assert_eq!(refused.hub, None);
        let allowed = route(&ns(), &[root(), lookup("ws-b"), getattr()]);
        assert_eq!(allowed.hub, Some(0xB));
    }

    #[test]
    fn a_leaked_filehandle_does_not_bypass_the_allowlist() {
        // The allowlist's second point: PUTFH of a hub handle. Without it,
        // a handle learned elsewhere skips the LOOKUP check entirely.
        let leaked = route(&ns(), &[putfh(0xC, 5), getattr()]);
        assert_eq!(leaked.disp, vec![Refuse(Nfs4Status::Stale)]);
        assert_eq!(leaked.hub, None);
        let unknown = route(&ns(), &[putfh(0xD, 5), getattr()]);
        assert_eq!(leaked, unknown);
        // control
        let ok = route(&ns(), &[putfh(0xA, 5), getattr()]);
        assert_eq!(ok.disp, vec![Forward, Forward]);
        assert_eq!(ok.hub, Some(0xA));
        assert!(!ok.putrootfh);
    }

    #[test]
    fn a_handle_no_hub_minted_is_badhandle() {
        let p = route(&ns(), &[Operation::PutFh(Nfs4FileHandle { data: vec![9; 20] })]);
        assert_eq!(p.disp, vec![Refuse(Nfs4Status::BadHandle)]);
    }

    #[test]
    fn stateid_ops_route_by_the_hub_tag_and_never_to_a_refused_hub() {
        // The allowlist's third point, and H2's reason to exist.
        let p = route(&ns(), &[Operation::FreeStateId(stateid(11))]);
        assert_eq!((p.disp, p.hub), (vec![Forward], Some(0xB)));
        let p = route(&ns(), &[Operation::FreeStateId(stateid(12))]);
        assert_eq!((p.disp, p.hub), (vec![Refuse(Nfs4Status::BadStateId)], None));
        let p = route(&ns(), &[Operation::TestStateId(vec![stateid(10), stateid(10)])]);
        assert_eq!((p.disp, p.hub), (vec![Forward], Some(0xA)));
        // no stateid names a hub it may use: answered here
        let p = route(&ns(), &[Operation::TestStateId(vec![stateid(12), stateid(99)])]);
        assert_eq!((p.disp, p.hub), (vec![Local], None));
    }

    #[test]
    fn a_test_stateid_spanning_two_hubs_is_a_counted_refusal() {
        let p = route(&ns(), &[Operation::TestStateId(vec![stateid(10), stateid(11)])]);
        assert_eq!(p.disp, vec![Refuse(Nfs4Status::ServerFault)]);
        assert!(p.second_target);
    }

    #[test]
    fn a_rename_across_workspaces_is_xdev_and_within_one_forwards() {
        let across = route(&ns(), &[putfh(0xA, 1), Operation::SaveFh, putfh(0xB, 2), rename()]);
        // §3: XDEV at the rename, not a counted second-target refusal
        assert_eq!(across.disp, vec![Forward, Forward, Local, Refuse(Nfs4Status::XDev)]);
        assert_eq!(across.hub, Some(0xA));
        assert!(!across.second_target);
        // a second hub with anything else behind it IS the counted one
        let other = route(&ns(), &[putfh(0xA, 1), Operation::SaveFh, putfh(0xB, 2), getattr()]);
        assert_eq!(other.disp.last(), Some(&Refuse(Nfs4Status::ServerFault)));
        assert!(other.second_target);
        let within = route(&ns(), &[putfh(0xA, 1), Operation::SaveFh, putfh(0xA, 2), rename()]);
        assert_eq!(within.disp, vec![Forward; 4]);
        assert_eq!(within.hub, Some(0xA));
        // a rename whose source is the pseudo-root: XDEV at the rename
        let from_root = route(&ns(), &[root(), Operation::SaveFh, putfh(0xA, 2), rename()]);
        assert_eq!(from_root.disp.last(), Some(&Refuse(Nfs4Status::XDev)));
        assert!(!from_root.second_target);
    }

    #[test]
    fn going_back_to_the_root_after_forwarding_is_a_counted_refusal() {
        let p = route(&ns(), &[putfh(0xA, 1), getattr(), Operation::PutRootFh, getattr()]);
        assert_eq!(p.disp, vec![Forward, Forward, Refuse(Nfs4Status::ServerFault)]);
        assert!(p.second_target);
        // a proxy-only prefix THEN one hub is the supported shape
        let p = route(&ns(), &[Operation::PutRootFh, getattr(), putfh(0xA, 1), getattr()]);
        assert_eq!(p.disp, vec![Local, Local, Forward, Forward]);
        assert!(!p.second_target);
    }

    #[test]
    fn lookupp_at_a_hub_root_comes_home_without_touching_the_hub() {
        let p = route(&ns(), &[putfh(0xA, 0), Operation::LookupP, Operation::GetFh]);
        assert_eq!(p.disp, vec![Local, Local, Local]);
        assert_eq!(p.hub, None);
        let p = route(&ns(), &[root(), lookup("ws-a"), Operation::LookupP, getattr()]);
        assert_eq!(p.disp, vec![Local, Local, Local, Local]);
        assert_eq!(p.hub, None);
        assert!(!p.putrootfh);
        // below the root it is the hub's LOOKUPP
        let p = route(&ns(), &[putfh(0xA, 3), Operation::LookupP]);
        assert_eq!(p.disp, vec![Forward, Forward]);
        // and at the pseudo-root, NOENT (RFC 8881 §18.14)
        let p = route(&ns(), &[Operation::PutRootFh, Operation::LookupP]);
        assert_eq!(p.disp, vec![Local, Refuse(Nfs4Status::NoEnt)]);
    }

    #[test]
    fn a_hub_root_putfh_with_work_behind_it_is_forwarded() {
        let p = route(&ns(), &[putfh(0xA, 0), getattr()]);
        assert_eq!(p.disp, vec![Forward, Forward]);
        assert_eq!(p.hub, Some(0xA));
        let p = route(&ns(), &[putfh(0xA, 0)]);
        assert_eq!(p.disp, vec![Forward]);
    }

    #[test]
    fn an_undecodable_op_takes_the_rest_opaque_to_the_current_target() {
        let p = route(&ns(), &[putfh(0xA, 1), getattr(), Operation::Unsupported(72)]);
        assert_eq!(p.disp, vec![Forward, Forward, Forward]);
        let p = route(&ns(), &[Operation::PutRootFh, Operation::Unsupported(72)]);
        assert_eq!(p.disp, vec![Local, Local]);
        assert_eq!(p.hub, None);
    }

    #[test]
    fn session_ops_are_the_proxys_and_never_follow_forwarded_ops() {
        let p = route(&ns(), &[Operation::ReclaimComplete(false)]);
        assert_eq!(p.disp, vec![Session]);
        let p = route(&ns(), &[putfh(0xA, 1), Operation::ReclaimComplete(false)]);
        assert_eq!(p.disp, vec![Forward, Refuse(Nfs4Status::ServerFault)]);
        assert!(p.second_target);
    }
}
