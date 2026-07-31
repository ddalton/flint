#!/usr/bin/env python3
"""A synthetic NFSv4.1 / pNFS client, for states a real kernel won't hold.

WHY. Every F65 defect (C1 seqid, C3 ack accounting, C8 AUTH_NONE, C9
csr_flags) needed a client holding a layout at the instant of a truncate.
A Linux client returns its layout ~80 ms after each I/O, so the live drill
had to coax it with O_DIRECT read loops, and two earlier attempts silently
held nothing at all. That is a race we keep re-losing.

This client holds a layout because it simply never returns one. No page
cache, no return_on_close, no 80 ms window. It also answers CB_LAYOUTRECALL
on the back channel, which is what makes "recalled, then asks again"
reproducible — the exact state R1 is about.

DELIBERATELY HAND-ROLLED XDR. It would be less code to import flint's own
encoders, and it would prove nothing: a client built from the server's
beliefs agrees with the server by construction. That is how
`assert_eq!(flavor, AuthFlavor::Null)` guarded the AUTH_NONE bug for
months. This file encodes RFC 8881, not flint.

WHAT IT IS NOT. It is not a conformance oracle. It shares no code with
Linux, but it does share an author with the server, so it cannot catch an
assumption I hold on both sides — C8 and C9 were both found by a real
kernel refusing us, and only a real kernel can find the next one. Use this
for server STATE MACHINE questions (does the gate park? for how long? what
does the recalled client get?), and keep truncate-recall.sh for the wire.
"""
import argparse
import os
import socket
import struct
import sys
import threading
import time

# ── NFSv4.1 operation numbers (RFC 8881 §16) ────────────────────────────
OP_CLOSE, OP_GETFH, OP_LOOKUP, OP_OPEN = 4, 10, 15, 18
OP_PUTFH, OP_PUTROOTFH, OP_READDIR, OP_SETATTR = 22, 24, 26, 34
OP_EXCHANGE_ID, OP_CREATE_SESSION, OP_DESTROY_SESSION = 42, 43, 44
OP_GETATTR = 9
OP_LAYOUTGET, OP_LAYOUTRETURN = 50, 51
OP_SEQUENCE, OP_RECLAIM_COMPLETE, OP_DESTROY_CLIENTID = 53, 58, 57
OP_CB_LAYOUTRECALL, OP_CB_SEQUENCE = 5, 11

NFS4_OK = 0
ERRS = {
    0: "NFS4_OK", 2: "NFS4ERR_NOENT", 13: "NFS4ERR_ACCES",
    10003: "NFS4ERR_BAD_COOKIE", 10004: "NFS4ERR_NOTSUPP",
    10005: "NFS4ERR_TOOSMALL", 10006: "NFS4ERR_SERVERFAULT",
    10008: "NFS4ERR_DELAY", 10011: "NFS4ERR_EXPIRED",
    10013: "NFS4ERR_GRACE", 10018: "NFS4ERR_RESOURCE",
    10020: "NFS4ERR_NOFILEHANDLE", 10022: "NFS4ERR_STALE_CLIENTID",
    10024: "NFS4ERR_OLD_STATEID", 10025: "NFS4ERR_BAD_STATEID",
    10036: "NFS4ERR_BADXDR", 10044: "NFS4ERR_OP_ILLEGAL",
    10047: "NFS4ERR_ADMIN_REVOKED", 10048: "NFS4ERR_CB_PATH_DOWN",
    10049: "NFS4ERR_BADIOMODE", 10050: "NFS4ERR_BADLAYOUT",
    10052: "NFS4ERR_BADSESSION", 10053: "NFS4ERR_BADSLOT",
    10055: "NFS4ERR_CONN_NOT_BOUND_TO_SESSION",
    10058: "NFS4ERR_LAYOUTTRYLATER", 10059: "NFS4ERR_LAYOUTUNAVAILABLE",
    10060: "NFS4ERR_NOMATCHING_LAYOUT", 10061: "NFS4ERR_RECALLCONFLICT",
    10062: "NFS4ERR_UNKNOWN_LAYOUTTYPE", 10063: "NFS4ERR_SEQ_MISORDERED",
    10064: "NFS4ERR_SEQUENCE_POS", 10068: "NFS4ERR_RETRY_UNCACHED_REP",
    10071: "NFS4ERR_OP_NOT_IN_SESSION", 10078: "NFS4ERR_DEADSESSION",
    10080: "NFS4ERR_PNFS_NO_LAYOUT",
}
# The layout "come back later" code is LAYOUTTRYLATER = 10058. Getting this
# wrong matters: an R1 verdict hinges on telling TRYLATER apart from a hard
# failure, and an unknown code prints as NFS4ERR?N rather than silently
# reading as something benign.
LAYOUTTRYLATER = 10058
NFS4ERR_DELAY = 10008

def errname(s):
    return ERRS.get(s, f"NFS4ERR?{s}")

LAYOUT4_NFSV4_1_FILES = 1
LAYOUTIOMODE4_READ, LAYOUTIOMODE4_RW = 1, 2


# ── XDR primitives ──────────────────────────────────────────────────────
def u32(v): return struct.pack(">I", v)
def u64(v): return struct.pack(">Q", v)
def opaque(b):
    pad = (4 - len(b) % 4) % 4
    return u32(len(b)) + b + b"\0" * pad
def fixed(b): return b


class Reader:
    def __init__(self, b):
        self.b, self.o = b, 0
    def u32(self):
        v = struct.unpack_from(">I", self.b, self.o)[0]; self.o += 4; return v
    def u64(self):
        v = struct.unpack_from(">Q", self.b, self.o)[0]; self.o += 8; return v
    def opaque(self):
        n = self.u32(); v = self.b[self.o:self.o + n]
        self.o += n + ((4 - n % 4) % 4); return v
    def fixed(self, n):
        v = self.b[self.o:self.o + n]; self.o += n; return v
    def left(self):
        return len(self.b) - self.o


class Nfs41Client:
    """One TCP connection: forward channel + (optionally) back channel."""

    def __init__(self, host, port=2049, verbose=False, deaf=False):
        # deaf=True: accept callbacks and NEVER answer them. Models a
        # client that is wedged or whose host has gone away with the TCP
        # connection still open — the case that costs the server a full
        # CB timeout rather than a fast broken-pipe.
        self.sock = socket.create_connection((host, port), timeout=30)
        self.sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self.xid = 1000
        self.verbose = verbose
        self.deaf = deaf
        self.clientid = None
        self.seqid = None
        self.sessionid = None
        self.slot_seq = 0
        self.cb_calls = []            # decoded CB_COMPOUNDs we answered
        self._pending = {}            # xid -> reply bytes
        self._lock = threading.Lock()
        self._cv = threading.Condition(self._lock)
        self._stop = False
        self._rx = threading.Thread(target=self._reader_loop, daemon=True)
        self._rx.start()

    # ── framing ────────────────────────────────────────────────────────
    def _send_record(self, payload):
        self.sock.sendall(struct.pack(">I", 0x80000000 | len(payload)) + payload)

    def _recv_exact(self, n):
        buf = b""
        while len(buf) < n:
            c = self.sock.recv(n - len(buf))
            if not c:
                raise ConnectionError("server closed the connection")
            buf += c
        return buf

    def _reader_loop(self):
        """One reader owns the socket. Forward replies get handed to the
        waiting caller; inbound CALLs are callbacks and are answered here,
        because the back channel shares this connection (CONN_BACK_CHAN)."""
        try:
            while not self._stop:
                marker = struct.unpack(">I", self._recv_exact(4))[0]
                body = self._recv_exact(marker & 0x7FFFFFFF)
                xid, mtype = struct.unpack_from(">II", body, 0)
                if mtype == 1:                       # REPLY to us
                    with self._cv:
                        self._pending[xid] = body
                        self._cv.notify_all()
                else:                                # CALL from the server
                    self._handle_callback(xid, body)
        except Exception as e:
            with self._cv:
                self._stop = True
                self._cv.notify_all()
            if self.verbose:
                print(f"    [reader stopped: {e}]", file=sys.stderr)

    def _await(self, xid, timeout=30):
        deadline = time.time() + timeout
        with self._cv:
            while xid not in self._pending:
                if self._stop:
                    raise ConnectionError("connection died awaiting reply")
                if not self._cv.wait(timeout=max(0.05, deadline - time.time())):
                    if time.time() > deadline:
                        raise TimeoutError(f"no reply to xid {xid}")
            return self._pending.pop(xid)

    # ── COMPOUND ───────────────────────────────────────────────────────
    def compound(self, ops, with_sequence=True, timeout=30):
        """ops: list of (opnum, encoded_args). Returns list of (op, status, Reader)."""
        body = b""
        n = 0
        if with_sequence and self.sessionid is not None:
            self.slot_seq += 1
            body += u32(OP_SEQUENCE) + fixed(self.sessionid) + u32(self.slot_seq) \
                + u32(0) + u32(0) + u32(0)
            n += 1
        for op, args in ops:
            body += u32(op) + args
            n += 1

        cargs = opaque(b"") + u32(1) + u32(0) + u32(n) + body   # tag, minorver, cbident? -> see note
        # COMPOUND4args: tag<>, minorversion, argarray<>
        cargs = opaque(b"") + u32(1) + u32(n) + body

        self.xid += 1
        xid = self.xid
        call = u32(xid) + u32(0) + u32(2) + u32(100003) + u32(4) + u32(1)
        call += u32(0) + u32(0) + u32(0) + u32(0)          # AUTH_NONE cred+verf
        call += cargs
        self._send_record(call)

        reply = self._await(xid, timeout=timeout)
        r = Reader(reply)
        r.u32(); r.u32()                                   # xid, mtype
        rstat = r.u32()
        if rstat != 0:
            raise RuntimeError(f"RPC DENIED (reply_stat={rstat})")
        r.u32(); r.opaque()                                # verifier
        astat = r.u32()
        if astat != 0:
            raise RuntimeError(f"RPC not accepted (accept_stat={astat})")
        status = r.u32()
        r.opaque()                                         # tag
        nres = r.u32()
        out = []
        for _ in range(nres):
            op = r.u32()
            st = r.u32()
            # DECODE EACH RESULT'S PAYLOAD NOW. nfs_resop4 is
            # {resop, status, <op-specific body>} — reading only op+status
            # leaves the body in the stream, so the NEXT iteration reads
            # the body as if it were an opcode. SEQUENCE has a 36-byte
            # body, so with the implicit SEQUENCE every COMPOUND result
            # after the first was garbage (LOOKUP "succeeded" with fh=None).
            out.append((op, st, self._decode_result(op, st, r)))
            if st != NFS4_OK:
                break                                      # COMPOUND stops here
        return status, out

    def _decode_result(self, op, st, r):
        """Consume exactly this result's body; return what we care about."""
        if st != NFS4_OK:
            # Error results carry no body, except LAYOUTGET's
            # LAYOUTTRYLATER hint (bool, RFC 8881 §18.43.3).
            if op == OP_LAYOUTGET and st == LAYOUTTRYLATER and r.left() >= 4:
                return {"trylater_hint": r.u32()}
            return {}
        if op == OP_SEQUENCE:
            return {"sessionid": r.fixed(16).hex(), "seqid": r.u32(),
                    "slot": r.u32(), "highest": r.u32(),
                    "target_highest": r.u32(), "flags": r.u32()}
        if op == OP_GETFH:
            return {"fh": r.opaque()}
        if op == OP_EXCHANGE_ID:
            return {"_raw": r}          # caller decodes (variable tail)
        if op == OP_CREATE_SESSION:
            return {"_raw": r}
        if op == OP_LAYOUTGET:
            d = {"return_on_close": r.u32(), "stateid_seqid": r.u32(),
                 "stateid_other": r.fixed(12).hex()}
            n = r.u32()
            d["nsegments"] = n
            segs = []
            for _ in range(n):
                segs.append({"offset": r.u64(), "length": r.u64(),
                             "iomode": r.u32(), "type": r.u32()})
                r.opaque()              # layout body (device id + stripe info)
            d["segments"] = segs
            return d
        if op == OP_CLOSE:
            return {"stateid": r.fixed(16).hex()}
        if op == OP_SETATTR:
            n = r.u32()
            return {"attrsset": [r.u32() for _ in range(n)]}
        if op == OP_GETATTR:
            n = r.u32()
            bm = [r.u32() for _ in range(n)]
            return {"bitmap": bm, "attrs": r.opaque()}
        # PUTFH / PUTROOTFH / LOOKUP / RECLAIM_COMPLETE / LAYOUTRETURN(no body
        # when lrs_present=FALSE) carry nothing we need.
        if op == OP_LAYOUTRETURN:
            if r.left() >= 4:
                present = r.u32()
                if present:
                    r.fixed(16)
            return {}
        return {}

    # ── callback handling ──────────────────────────────────────────────
    def _handle_callback(self, xid, body):
        """Answer CB_COMPOUND. Answering is the point: R1 is about what a
        client sees AFTER it has been recalled, so refusing here would test
        a different (and easier) scenario."""
        if self.deaf:
            self.cb_calls.append({"op": "IGNORED"})
            if self.verbose:
                print("    [deaf: callback received, deliberately NOT answered]")
            return
        r = Reader(body)
        r.u32(); r.u32()                                   # xid, mtype(CALL)
        r.u32(); prog = r.u32(); r.u32(); proc = r.u32()   # rpcvers, prog, vers, proc
        r.u32(); r.opaque()                                # cred flavor+body
        r.u32(); r.opaque()                                # verf
        results = b""
        nres = 0
        seen = []
        try:
            r.opaque()                                     # tag
            r.u32()                                        # minorversion
            r.u32()                                        # callback_ident
            nops = r.u32()
            for _ in range(nops):
                op = r.u32()
                seen.append(op)
                if op == OP_CB_SEQUENCE:
                    sid = r.fixed(16); seq = r.u32(); slot = r.u32()
                    r.u32(); r.u32()                       # highest_slotid, cachethis
                    # csa_referring_call_lists<> — DO NOT SKIP. Leaving it
                    # in the stream makes the array count read as the next
                    # opcode, so the reply names op 0 instead of
                    # CB_LAYOUTRECALL. flint then (correctly, post-C3)
                    # scores the recall as "never ran" and reports
                    # "only 1/2 acked". The pre-C3 server would have
                    # called that a clean ack — this client reproduced
                    # the exact bug C3 was written to catch, and C3
                    # caught it.
                    for _ in range(r.u32()):               # referring_call_list4
                        r.fixed(16)                        # rcl_sessionid
                        for _ in range(r.u32()):           # rcl_referring_calls<>
                            r.u32(); r.u32()               # seqid, slotid
                    results += u32(OP_CB_SEQUENCE) + u32(NFS4_OK) + fixed(sid) \
                        + u32(seq) + u32(slot) + u32(0) + u32(0)
                    nres += 1
                elif op == OP_CB_LAYOUTRECALL:
                    ltype = r.u32(); iomode = r.u32(); changed = r.u32()
                    rectype = r.u32()
                    info = {}
                    # layoutrecall_type4: FILE=1, FSID=2, ALL=3 (RFC 8881
                    # §20.3.3). FILE is 1, not 0 — reading it as 0 silently
                    # skipped the fh/offset/length and recorded a recall
                    # with no detail, which is precisely the kind of
                    # "it worked" that hides what actually happened.
                    if rectype == 1:                        # LAYOUTRECALL4_FILE
                        fh = r.opaque(); off = r.u64(); ln = r.u64()
                        st_seq = r.u32(); st_other = r.fixed(12)
                        info = dict(fh=fh.hex()[:16], off=off, length=ln,
                                    stateid_seqid=st_seq)
                    self.cb_calls.append(dict(op="CB_LAYOUTRECALL", iomode=iomode,
                                              rectype=rectype, **info))
                    results += u32(OP_CB_LAYOUTRECALL) + u32(NFS4_OK)
                    nres += 1
                else:
                    results += u32(op) + u32(NFS4_OK)
                    nres += 1
        except Exception as e:
            if self.verbose:
                print(f"    [cb decode stopped: {e} after ops {seen}]", file=sys.stderr)

        rep = u32(xid) + u32(1) + u32(0) + u32(0) + u32(0) + u32(0)
        rep += u32(NFS4_OK) + opaque(b"") + u32(nres) + results
        self._send_record(rep)
        if self.verbose:
            print(f"    [answered callback: ops={seen}]")

    # ── operations ─────────────────────────────────────────────────────
    def exchange_id(self, owner=None):
        # A FRESH clientowner per run. Reusing it makes EXCHANGE_ID return
        # the previously confirmed clientid, whose CREATE_SESSION sequence
        # this process does not know — the server then answers
        # SEQ_MISORDERED, which reads like a client bug and is really just
        # state left over from the last run.
        if owner is None:
            owner = b"flint-synth-" + os.urandom(8).hex().encode()
        # eia_clientowner = { verifier<8>, ownerid<> }
        args = os.urandom(8) + opaque(owner)
        args += u32(0) + u32(0) + u32(0)     # flags, state_protect(SP4_NONE), impl_id<>
        st, res = self.compound([(OP_EXCHANGE_ID, args)], with_sequence=False)
        if st != NFS4_OK:
            raise RuntimeError(f"EXCHANGE_ID: {errname(st)}")
        r = res[-1][2]["_raw"]
        self.clientid = r.u64()
        self.seqid = r.u32()
        return self.clientid

    def create_session(self, back_chan=True, cb_program=0x40000000):
        flags = 0x2 if back_chan else 0
        fore = u32(0) + u32(1 << 20) + u32(1 << 20) + u32(64 * 1024) + u32(8) + u32(64) + u32(0)
        back = u32(0) + u32(4096) + u32(4096) + u32(0) + u32(2) + u32(1) + u32(0)
        args = u64(self.clientid) + u32(self.seqid) + u32(flags) + fore + back
        args += u32(cb_program)
        args += u32(1) + u32(1) + u32(int(time.time()) & 0x7FFFFFFF) \
            + opaque(b"flint-synth") + u32(0) + u32(0) + u32(0)   # csa_sec_parms<1>: AUTH_SYS
        st, res = self.compound([(OP_CREATE_SESSION, args)], with_sequence=False)
        if st != NFS4_OK:
            raise RuntimeError(f"CREATE_SESSION: {errname(st)}")
        r = res[-1][2]["_raw"]
        self.sessionid = r.fixed(16)
        r.u32()                                            # csr_sequence
        csr_flags = r.u32()
        self.slot_seq = 0
        return csr_flags

    def reclaim_complete(self):
        st, _ = self.compound([(OP_RECLAIM_COMPLETE, u32(0))])
        return st

    def lookup_path(self, parts):
        ops = [(OP_PUTROOTFH, b"")]
        for p in parts:
            ops.append((OP_LOOKUP, opaque(p.encode())))
        ops.append((OP_GETFH, b""))
        st, res = self.compound(ops)
        if st != NFS4_OK:
            return st, None
        for op, s, d in res:
            if op == OP_GETFH and s == NFS4_OK:
                return st, d["fh"]
        return st, None

    def layoutget(self, fh, iomode=LAYOUTIOMODE4_RW, offset=0,
                  length=0xFFFFFFFFFFFFFFFF, stateid=None, timeout=30):
        """Returns (compound_status, layout_status, info). Never returns the
        layout — holding it is the whole point of this client."""
        sid = stateid or (u32(1) + b"\0" * 12)
        args = u32(0)                       # loga_signal_layout_avail = FALSE
        args += u32(LAYOUT4_NFSV4_1_FILES) + u32(iomode)
        args += u64(offset) + u64(length) + u64(4096)
        args += fixed(sid) + u32(8)         # maxcount
        st, res = self.compound([(OP_PUTFH, opaque(fh)), (OP_LAYOUTGET, args)],
                                timeout=timeout)
        lg = next(((s, d) for op, s, d in res if op == OP_LAYOUTGET), None)
        if lg is None:
            return st, None, {}
        return st, lg[0], lg[1]

    def close(self, destroy=True):
        """Tear the session down. Without this each run leaves a session
        that still counts as a layout HOLDER, so later runs report
        "only N/M acked" for clients that are just abandoned test state —
        noise that looks exactly like a real unacked recall."""
        if destroy and self.sessionid is not None:
            try:
                self.compound([(OP_DESTROY_SESSION, fixed(self.sessionid))],
                              with_sequence=False, timeout=5)
            except Exception:
                pass
        if destroy and self.clientid is not None:
            try:
                self.compound([(OP_DESTROY_CLIENTID, u64(self.clientid))],
                              with_sequence=False, timeout=5)
            except Exception:
                pass
        self._stop = True
        try:
            self.sock.close()
        except Exception:
            pass


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=2049)
    ap.add_argument("--path", default="", help="export-relative path, slash separated")
    ap.add_argument("--verbose", action="store_true")
    a = ap.parse_args()

    c = Nfs41Client(a.host, a.port, verbose=a.verbose)
    print(f"EXCHANGE_ID  clientid=0x{c.exchange_id():x} seqid={c.seqid}")
    fl = c.create_session()
    print(f"CREATE_SESSION sessionid={c.sessionid.hex()} csr_flags=0x{fl:x} "
          f"CONN_BACK_CHAN={'YES' if fl & 2 else 'NO'}")
    print(f"RECLAIM_COMPLETE {errname(c.reclaim_complete())}")
    parts = [p for p in a.path.split("/") if p]
    st, fh = c.lookup_path(parts)
    print(f"LOOKUP {'/'.join(parts) or '<root>'} -> {errname(st)} fh={fh.hex()[:24] if fh else None}")
    c.close()


if __name__ == "__main__":
    main()
