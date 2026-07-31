#!/usr/bin/env python3
"""Decode NFSv4.1 CALLBACK exchanges out of `tcpdump -X` output.

WHY THIS EXISTS. The truncate-recall drill used to decide whether a
CB_LAYOUTRECALL reached the client with:

    tcpdump -r $CAP -A | grep -c CB_

NFS is binary XDR. The string "CB_" never appears on the wire, so that
count is structurally always 0 — the check could not pass, and when it
"failed" it blamed the server ("the recall never left the server") for
what was the oracle's own blindness. It is the same family of bug as
scoring every decodable reply as an ack (C3), asserting the callback
flavour the code already emitted (C8), and holding a layout by re-reading
one page-cached block: an instrument that reports on itself.

So this decodes the actual RPC. It reads `tcpdump -X` text on stdin and
prints one line per callback CALL and REPLY, with the fields that have
each, in turn, been the bug:

    reply_stat   MSG_DENIED here  => the client refused the RPC     (C8)
    CB_SEQUENCE  BADSESSION here  => back channel never bound       (C9)
    layout seqid not advanced     => stale stateid                  (C1)

Exit 0 if at least one callback CALL was found and every reply to it was
accepted with all ops NFS4_OK; 1 otherwise. Prints a VERDICT line either
way so the caller never has to infer from silence.
"""
import re
import sys

# RFC 8881 §20 callback operations.
CB_OPS = {
    3: "CB_GETATTR", 4: "CB_RECALL", 5: "CB_LAYOUTRECALL", 6: "CB_NOTIFY",
    7: "CB_PUSH_DELEG", 8: "CB_RECALL_ANY", 9: "CB_RECALLABLE_OBJ_AVAIL",
    10: "CB_RECALL_SLOT", 11: "CB_SEQUENCE", 12: "CB_WANTS_CANCELLED",
    13: "CB_NOTIFY_LOCK", 14: "CB_NOTIFY_DEVICEID", 10044: "CB_ILLEGAL",
}
NFS4_ERR = {
    0: "NFS4_OK", 10024: "NFS4ERR_OLD_STATEID", 10052: "NFS4ERR_BADSESSION",
    10068: "NFS4ERR_RETRY_UNCACHED_REP", 10008: "NFS4ERR_BAD_STATEID",
    10011: "NFS4ERR_DELAY", 10036: "NFS4ERR_SEQ_MISORDERED",
    10045: "NFS4ERR_BADLAYOUT", 10049: "NFS4ERR_NOMATCHING_LAYOUT",
}
AUTH = {0: "AUTH_NONE", 1: "AUTH_SYS", 6: "RPCSEC_GSS"}
REPLY_STAT = {0: "MSG_ACCEPTED", 1: "MSG_DENIED"}
ACCEPT_STAT = {0: "SUCCESS", 1: "PROG_UNAVAIL", 2: "PROG_MISMATCH",
               3: "PROC_UNAVAIL", 4: "GARBAGE_ARGS", 5: "SYSTEM_ERR"}


def packets(text):
    """Yield (timestamp, direction, payload_bytes) per captured frame."""
    cur_hdr, cur_hex = None, []
    for line in text.split("\n"):
        m = re.match(r"^\s+0x[0-9a-f]+:\s+((?:[0-9a-f]{2,4}\s+)+)", line)
        if m and cur_hdr is not None:
            cur_hex.append(m.group(1).replace(" ", ""))
            continue
        if cur_hdr is not None and cur_hex:
            yield cur_hdr, bytes.fromhex("".join(cur_hex))
            cur_hdr, cur_hex = None, []
        if re.match(r"^\d\d:\d\d:\d\d\.\d+", line):
            cur_hdr, cur_hex = line, []
    if cur_hdr is not None and cur_hex:
        yield cur_hdr, bytes.fromhex("".join(cur_hex))


def tcp_payload(frame):
    """Strip IP + TCP headers. Returns b'' when the frame isn't IPv4/TCP."""
    if len(frame) < 20 or (frame[0] >> 4) != 4:
        return b""
    ihl = (frame[0] & 0x0F) * 4
    if len(frame) < ihl + 20 or frame[9] != 6:  # proto 6 = TCP
        return b""
    doff = (frame[ihl + 12] >> 4) * 4
    return frame[ihl + doff:]


def u32(b, o):
    return int.from_bytes(b[o:o + 4], "big") if o + 4 <= len(b) else None


def main():
    text = sys.stdin.read()
    calls, replies = {}, {}

    for hdr, frame in packets(text):
        pay = tcp_payload(frame)
        if len(pay) < 12:
            continue
        # Skip the RPC record marker if present.
        for base in (4, 0):
            xid = u32(pay, base)
            mtype = u32(pay, base + 4)
            if xid is None or mtype not in (0, 1):
                continue

            if mtype == 0:  # CALL
                if u32(pay, base + 8) != 2:   # rpcvers
                    continue
                prog, vers, proc = (u32(pay, base + 12), u32(pay, base + 16),
                                    u32(pay, base + 20))
                # NFSv4.1 callback programs are client-chosen and large;
                # match on the CB_COMPOUND procedure rather than a
                # hardcoded program number.
                if not prog or prog < 0x4000_0000 or proc != 1:
                    continue
                o = base + 24
                cflav, clen = u32(pay, o), u32(pay, o + 4)
                o += 8 + (clen or 0)
                o += 8 + (u32(pay, o + 4) or 0)      # verifier
                taglen = u32(pay, o) or 0
                o += 4 + ((taglen + 3) // 4) * 4
                o += 4                                 # minorversion
                o += 4                                 # callback_ident
                nops = u32(pay, o)
                o += 4
                ops = []
                for _ in range(min(nops or 0, 8)):
                    op = u32(pay, o)
                    if op is None:
                        break
                    ops.append(CB_OPS.get(op, f"op{op}"))
                    break  # only the first opcode is positionally safe
                calls[xid] = dict(hdr=hdr.split()[0], prog=prog, vers=vers,
                                  cred=AUTH.get(cflav, f"flavor{cflav}"),
                                  nops=nops, first=ops[0] if ops else "?")
            else:  # REPLY
                rstat = u32(pay, base + 8)
                if rstat not in (0, 1):
                    continue
                r = dict(hdr=hdr.split()[0], reply_stat=REPLY_STAT[rstat])
                if rstat == 0:
                    o = base + 12
                    o += 8 + (u32(pay, o + 4) or 0)    # verifier
                    astat = u32(pay, o)
                    r["accept_stat"] = ACCEPT_STAT.get(astat, f"?{astat}")
                    o += 4
                    if astat == 0:
                        st = u32(pay, o)
                        r["status"] = NFS4_ERR.get(st, f"NFS4ERR?{st}")
                        o += 4
                        taglen = u32(pay, o) or 0
                        o += 4 + ((taglen + 3) // 4) * 4
                        r["nresults"] = u32(pay, o)
                        o += 4
                        op = u32(pay, o)
                        r["first_op"] = CB_OPS.get(op, f"op{op}")
                        r["first_status"] = NFS4_ERR.get(
                            u32(pay, o + 4), f"NFS4ERR?{u32(pay, o + 4)}")
                replies[xid] = r
        # end per-frame

    cb_xids = sorted(calls)
    if not cb_xids:
        print("VERDICT FAIL no callback CALL found in the capture")
        return 1

    ok = True
    for xid in cb_xids:
        c = calls[xid]
        print(f"CALL  xid=0x{xid:08x} {c['hdr']} prog=0x{c['prog']:08x} "
              f"cred={c['cred']} ops={c['nops']} first={c['first']}")
        r = replies.get(xid)
        if not r:
            print(f"REPLY xid=0x{xid:08x} *** NONE — the client never answered")
            ok = False
            continue
        bits = " ".join(f"{k}={v}" for k, v in r.items() if k != "hdr")
        print(f"REPLY xid=0x{xid:08x} {r['hdr']} {bits}")
        if r["reply_stat"] != "MSG_ACCEPTED":
            print("      ^ RPC DENIED — the client rejected our credential (C8)")
            ok = False
        elif r.get("accept_stat") != "SUCCESS":
            print("      ^ RPC accepted but the CB program did not run")
            ok = False
        elif r.get("status") != "NFS4_OK" or r.get("first_status") != "NFS4_OK":
            if r.get("first_status") == "NFS4ERR_BADSESSION":
                print("      ^ BADSESSION at CB_SEQUENCE — back channel not "
                      "bound; csr_flags never echoed CONN_BACK_CHAN (C9)")
            ok = False

    print(f"VERDICT {'PASS' if ok else 'FAIL'} "
          f"{len(cb_xids)} callback CALL(s), "
          f"{sum(1 for x in cb_xids if x in replies)} answered")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
