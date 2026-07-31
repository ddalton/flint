#!/usr/bin/env python3
"""In-cluster R1 probe: hold a layout, take the recall, then poll.

Runs INSIDE the cluster, unlike r1-parked-truncate.py which drives the
client from a laptop through `kubectl port-forward`. That mattered: under
the ~10 s stall a parked truncate imposes on the truncating client, the
port-forward reset the connection, so the fault arm never actually
received its CB_LAYOUTRECALL and measured "a client with no layout" rather
than "a RECALLED client". Same LAYOUTGET path, weaker claim.

Here the socket is a plain pod-to-pod connection to the MDS Service, so
the recall arrives and is answered, and the poll survives a multi-minute
park.

Emits one line per state change, flushed, so an external orchestrator can
scale DSes around it and read the timeline live.

  python3 r1_probe_inpod.py --host flint-pnfs-mds --pvc <pvc> --file f.bin \
      --poll-secs 300
"""
import argparse
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import synth_client as sc          # noqa: E402


def say(msg):
    print(f"[{time.time():.3f}] {msg}", flush=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="flint-pnfs-mds")
    ap.add_argument("--port", type=int, default=2049)
    ap.add_argument("--pvc", required=True)
    ap.add_argument("--file", required=True)
    ap.add_argument("--poll-secs", type=float, default=300)
    a = ap.parse_args()

    c = sc.Nfs41Client(a.host, a.port)
    c.exchange_id()
    flags = c.create_session()
    say(f"SESSION csr_flags=0x{flags:x} back_chan={'YES' if flags & 2 else 'NO'}")
    if not flags & 2:
        say("FATAL no back channel — a recall can never arrive")
        return 2
    c.reclaim_complete()

    st, fh = c.lookup_path([a.pvc, a.file])
    if st != sc.NFS4_OK or not fh:
        say(f"FATAL lookup {sc.errname(st)}")
        return 2
    _, lst, info = c.layoutget(fh)
    if lst != sc.NFS4_OK:
        say(f"FATAL initial layoutget {sc.errname(lst)}")
        return 2
    say(f"HOLDING layout seqid={info['stateid_seqid']} segments={info['nsegments']}")
    say("READY")                      # orchestrator's cue to truncate

    t0 = time.time()
    seen_recall = False
    last = None
    trylater_since = None
    while time.time() - t0 < a.poll_secs:
        if c.cb_calls and not seen_recall:
            seen_recall = True
            say(f"RECALL received+answered after {time.time()-t0:.2f}s: {c.cb_calls[0]}")
        try:
            _, s, _ = c.layoutget(fh, timeout=20)
        except Exception as e:
            say(f"ERROR {type(e).__name__}: {e}")
            time.sleep(1)
            continue
        name = sc.errname(s) if s is not None else "NO_LAYOUTGET_RESULT"
        if name != last:
            say(f"LAYOUTGET {name} (t+{time.time()-t0:.2f}s)")
            if name == "NFS4ERR_LAYOUTTRYLATER" and trylater_since is None:
                trylater_since = time.time()
            if name == "NFS4_OK" and trylater_since is not None:
                say(f"RECOVERED after {time.time()-trylater_since:.2f}s of TRYLATER")
                trylater_since = None
            last = name
        time.sleep(0.5)

    if trylater_since is not None:
        say(f"STILL TRYLATER after {time.time()-trylater_since:.2f}s — "
            f"never converted to an error")
    say("DONE")
    c.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
