#!/usr/bin/env python3
"""One phase of design §9's restart legs, driven across a server restart.

This cannot be a pynfs test module: the whole point is what happens
BETWEEN two server incarnations, and a test function runs inside one.
So the shell rig restarts the server and calls this twice.

The client owner AND verifier are pinned to constants, because that is
what makes the second EXCHANGE_ID land in case 1 (same owner + same
verifier => same confirmed clientid). pynfs otherwise derives the
verifier from time.time(), which differs per process and would present
as a client REBOOT — a different code path entirely, and one that would
make this leg pass for the wrong reason.

Phases:
  hold    establish a client, take a READ delegation, exit holding it
  probe   re-establish the SAME client and report what the server says
  fresh   establish a DIFFERENT client and report the same fields
          (the control: the SEQ4 bit must NOT be set for a client that
          never held anything across the restart)

Output is one JSON object on stdout so the rig can assert on it.
"""
import json
import sys

sys.path.insert(0, "/opt/pynfs/nfs4.1")
sys.path.insert(0, "/opt/pynfs")

from xdrdef.nfs4_const import *          # noqa: E402
from xdrdef.nfs4_type import *           # noqa: E402
import nfs4client                        # noqa: E402
import nfs4lib                           # noqa: E402
import nfs_ops                           # noqa: E402
import rpc.security                      # noqa: E402

op = nfs_ops.NFS4ops()

PHASE = sys.argv[1]
HOST = sys.argv[2]
PORT = int(sys.argv[3])
EXPORT = sys.argv[4].encode()
FNAME = sys.argv[5].encode()

OWNER = b"flint-restart-leg-holder"
OWNER_FRESH = b"flint-restart-leg-fresh"
# The grace probe needs its OWN identity. Using the holder's would make
# it establish a session as the holder, and the SEQ4 bit leg (a) is
# waiting for is delivered on that client's FIRST SEQUENCE and lowered
# as soon as the next one acks it — so the grace check would silently
# consume the very evidence leg (a) exists to observe.
OWNER_GRACE = b"flint-restart-leg-grace"
VERF = b"FLINTv001"[:8]

out = {"phase": PHASE}


def connect(owner):
    c = nfs4client.NFS4Client(HOST, PORT, minorversion=1)
    c.set_cred(rpc.security.AuthSys().init_cred(
        uid=0, gid=0, name=b"flintrig"))
    cl = c.new_client(owner, verf=VERF)
    return c, cl


def path_ops(sess):
    """PUTROOTFH + LOOKUPs down to the export directory."""
    ops = [op.putrootfh()]
    for comp in EXPORT.strip(b"/").split(b"/"):
        if comp:
            ops.append(op.lookup(comp))
    return ops


if PHASE in ("hold", "probe"):
    owner = OWNER
elif PHASE == "fresh":
    owner = OWNER_FRESH
elif PHASE == "gracehold":
    owner = OWNER_GRACE
else:
    raise SystemExit("unknown phase %s" % PHASE)

c, cl = connect(owner)
out["clientid"] = cl.clientid
sess = cl.create_session()

# RECLAIM_COMPLETE only on the phase that needs it. It carries a
# SEQUENCE, and the SEQ4 bit this leg is looking for is delivered on the
# FIRST SEQUENCE and then LOWERED as soon as a later SEQUENCE on the
# same slot acks it (RFC 8881 §2.10.6.1 slot-ack, and
# `note_seq4_delivery` implements exactly that). Sending anything before
# the probe consumes the bit and the probe then reads a truthful zero
# off a server that behaved perfectly.
if PHASE in ("hold", "gracehold"):
    sess.compound([op.reclaim_complete(FALSE)])

if PHASE in ("hold", "gracehold"):
    # Create the file, then take the delegation on a NO-CREATE open:
    # design rule 3 means the CREATE arm never grants, and create_file's
    # default SHARE_ACCESS_BOTH would block the grant under rule 5.
    owner4 = open_owner4(0, b"restart leg owner")
    claim = open_claim4(CLAIM_NULL, FNAME)
    res = sess.compound(path_ops(sess) + [
        op.open(0, OPEN4_SHARE_ACCESS_READ, OPEN4_SHARE_DENY_NONE, owner4,
                openflag4(OPEN4_CREATE, createhow4(UNCHECKED4,
                          {FATTR4_MODE: 0o644})), claim),
        op.getfh()])
    out["create_status"] = res.status
    res = sess.compound(path_ops(sess) + [
        op.open(0, OPEN4_SHARE_ACCESS_READ | OPEN4_SHARE_ACCESS_WANT_READ_DELEG,
                OPEN4_SHARE_DENY_NONE, owner4,
                openflag4(OPEN4_NOCREATE), claim),
        op.getfh()])
    out["open_status"] = res.status
    if res.status == NFS4_OK:
        deleg = res.resarray[-2].delegation
        out["deleg_type"] = deleg.delegation_type
        if deleg.delegation_type == OPEN_DELEGATE_READ:
            out["deleg_stateid"] = list(deleg.read.stateid.other)
    # Exit HOLDING it. No DELEGRETURN, no CLOSE: the restart has to find
    # a live delegation, which is the whole precondition of the leg.

elif PHASE in ("probe", "fresh"):
    # THE FIRST SEQUENCE this client sends after re-establishing. Read
    # through the raw compound so the SEQUENCE result itself is visible:
    # sess.compound() strips it, and the status flags live there.
    slot, seq_op = sess._prepare_compound({})
    res = sess.c.compound([seq_op])
    flags = res.resarray[0].sr_status_flags
    out["seq_status_flags"] = flags
    out["recallable_state_revoked"] = bool(
        flags & SEQ4_STATUS_RECALLABLE_STATE_REVOKED)
    sess.update_seq_state(res, slot)

    if PHASE == "probe" and len(sys.argv) > 6:
        # TEST_STATEID on the delegation the previous incarnation gave
        # out: the server must not still call it good.
        other = bytes(json.loads(sys.argv[6]))
        sid = stateid4(1, other)
        r = sess.compound([op.test_stateid([sid])])
        if r.status == NFS4_OK:
            out["test_stateid_code"] = r.resarray[0].tsr_status_codes[0]
        else:
            out["test_stateid_compound_status"] = r.status

print(json.dumps(out))
