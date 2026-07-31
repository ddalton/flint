#!/usr/bin/env python3
"""How long does ONE truncate cost when layout holders do not answer?

`note_truncate` recalls before it fans out, and awaits each recall:

    for (session_id, stateid, fh) in recalls { ... .await }

Sequential, with a 10s DEFAULT_CB_TIMEOUT per call. So the SETATTR that
performs a truncate blocks for one callback round-trip per outstanding
layout — and a holder that is wedged (host gone, TCP still open) costs the
full timeout rather than a fast broken-pipe.

The per-session ordering is REQUIRED: a back channel negotiates
ca_maxrequests=1, so two concurrent CB_COMPOUNDs to one session would
collide on slot 0. But holders are different CLIENTS on different
SESSIONS, and nothing about slot 0 forces those to be serialized. If the
cost scales with holder count, that is the finding.

This is a state a real kernel will not enter on demand — hence a
synthetic client in --deaf mode, which accepts callbacks and never
answers.

  KUBECONFIG=... ./deaf-holders.py --pvc <pvc> --holders 1
  KUBECONFIG=... ./deaf-holders.py --pvc <pvc> --holders 3
"""
import argparse
import os
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import synth_client as sc          # noqa: E402


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="flint-pnfs-mds.flint-system.svc.cluster.local")
    ap.add_argument("--port", type=int, default=2049)
    ap.add_argument("--pvc", required=True)
    ap.add_argument("--file", required=True)
    ap.add_argument("--holders", type=int, default=1)
    ap.add_argument("--hold-secs", type=float, default=180)
    a = ap.parse_args()

    clients = []
    for i in range(a.holders):
        c = sc.Nfs41Client(a.host, a.port, deaf=True)
        c.exchange_id()
        f = c.create_session()
        assert f & 2, "no back channel — a deaf holder must still be reachable"
        c.reclaim_complete()
        st, fh = c.lookup_path([a.pvc, a.file])
        assert st == sc.NFS4_OK and fh, f"lookup: {sc.errname(st)}"
        _, lst, info = c.layoutget(fh)
        assert lst == sc.NFS4_OK, f"layoutget: {sc.errname(lst)}"
        clients.append(c)
        print(f"  holder {i+1}/{a.holders} HOLDING (deaf) seqid={info['stateid_seqid']}",
              flush=True)

    # This process runs INSIDE the cluster and has no kubectl. It only
    # establishes the holders and stays alive holding them; the caller
    # times the truncate from outside. Splitting it this way also keeps
    # the timing off the same host as the holders.
    print(f"READY {len(clients)} deaf holder(s) established", flush=True)
    time.sleep(a.hold_secs)
    print("RELEASING", flush=True)
    for c in clients:
        c.close(destroy=False)
    return 0


if __name__ == "__main__":
    sys.exit(main())
