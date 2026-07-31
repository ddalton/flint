#!/usr/bin/env python3
"""R1: what does a RECALLED client get while a truncate is PARKED?

R1 is the open question from the F65 audit: after a truncate recalls a
client's layout, that client has no layout, cannot get a new one while the
truncate gate is parked, and has no MDS data path. The audit predicted a
90 s DELAY ending in NFS4ERR_IO at fsync().

The live kernel drill on runat measured the park at 4.28 ms with exactly
one TRYLATER — but that only exercised the BENIGN branch, because the park
closes as soon as every DS confirms its stripe truncation. The dangerous
branch needs the park to PERSIST, i.e. a DS that cannot confirm.

Two arms, always run as an A/B — a fault-injection result with no baseline
is unreadable:

  baseline   both DSes healthy      → expect a park of milliseconds
  ds-down    one DS stopped         → expect the park to persist

The layout holder is the synthetic client, not a kernel mount, because a
kernel returns its layout ~80 ms after each I/O and this experiment needs
one held across an event we schedule. The client answers CB_LAYOUTRECALL
(that is the point — R1 is about the state AFTER a successful recall) and
then polls LAYOUTGET, recording every status with a timestamp.

  KUBECONFIG=... ./r1-parked-truncate.py --pvc <pvc-name> --arm both
"""
import argparse
import importlib.util
import json
import os
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("sc", os.path.join(HERE, "synth_client.py"))
sc = importlib.util.module_from_spec(spec)
spec.loader.exec_module(sc)

NS = os.environ.get("NS", "flint-system")


def kubectl(*args, check=True, timeout=180):
    r = subprocess.run(["kubectl", *args], capture_output=True, text=True, timeout=timeout)
    if check and r.returncode != 0:
        raise RuntimeError(f"kubectl {' '.join(args)}: {r.stderr.strip()}")
    return r.stdout.strip()


def ds_pods():
    out = kubectl("get", "pods", "-n", NS, "-l", "app=flint-pnfs-ds",
                  "-o", "jsonpath={range .items[*]}{.metadata.name} {.status.phase}{\"\\n\"}{end}")
    return [l.split() for l in out.split("\n") if l.strip()]


def run_arm(arm, host, port, pvc, fname, writer_pod, poll_secs, verbose, down_secs=60):
    print(f"\n{'='*66}\nARM: {arm}\n{'='*66}")

    # Fresh file per arm: a truncate is not idempotent, and reusing the
    # previous arm's zero-length file would make the second arm measure
    # nothing at all while looking like it passed.
    kubectl("exec", writer_pod, "--", "sh", "-c",
            f"dd if=/dev/urandom of=/data/{fname} bs=1M count=32 2>/dev/null; sync")
    size = kubectl("exec", writer_pod, "--", "sh", "-c", f"wc -c < /data/{fname}")
    print(f"  file /data/{fname} = {size.strip()} bytes")

    c = sc.Nfs41Client(host, port, verbose=verbose)
    c.exchange_id()
    flags = c.create_session()
    assert flags & 2, "server did not echo CONN_BACK_CHAN — no recall can arrive (C9)"
    c.reclaim_complete()
    st, fh = c.lookup_path([pvc, fname])
    if st != sc.NFS4_OK or not fh:
        raise RuntimeError(f"LOOKUP failed: {sc.errname(st)}")

    _, lst, info = c.layoutget(fh)
    if lst != sc.NFS4_OK:
        raise RuntimeError(f"initial LAYOUTGET failed: {sc.errname(lst)}")
    print(f"  layout HELD seqid={info['stateid_seqid']} segments={info['nsegments']}")

    stopped = None
    if arm == "ds-down":
        # Take one pinned DS away so the MDS cannot confirm stripe
        # truncation on every pinned DS, and the gate stays parked.
        #
        # NOT SIGSTOP. `kubectl exec -- kill -STOP 1` is silently ignored:
        # the kernel refuses signals whose default action would stop or
        # kill PID 1 when they come from inside that PID namespace. The
        # command exits 0, /proc/1/stat never leaves state S, and the
        # "fault" arm measures a perfectly healthy fleet while looking
        # like it injected something. Scaling the StatefulSet actually
        # removes the pod.
        n_before = len(ds_pods())
        kubectl("scale", "statefulset", "flint-pnfs-ds", "-n", NS, "--replicas=1")
        deadline = time.time() + 120
        while time.time() < deadline and len(ds_pods()) >= n_before:
            time.sleep(2)
        now = ds_pods()
        # VERIFY THE FAULT. An injection that did not take turns a fault
        # arm into a second baseline, and the two are indistinguishable
        # from the results alone — which is exactly what happened with
        # SIGSTOP.
        if len(now) >= n_before:
            raise RuntimeError(
                f"DS fleet did not shrink ({n_before} -> {len(now)}): the fault "
                f"never took hold, so this arm would silently be a second baseline")
        stopped = True
        print(f"  DS fleet {n_before} -> {len(now)} (a pinned DS is GONE, "
              f"stripe truncation cannot confirm)")

    restore_at = None
    if stopped:
        import threading
        def _restore():
            time.sleep(down_secs)
            kubectl("scale", "statefulset", "flint-pnfs-ds", "-n", NS,
                    "--replicas=2", check=False)
            print(f"  [t+{down_secs:.0f}s] DS fleet scaled back to 2")
        restore_at = threading.Thread(target=_restore, daemon=True)
        restore_at.start()

    try:
        t_trunc = time.time()
        kubectl("exec", writer_pod, "--", "sh", "-c",
                f"printf '' > /data/{fname}", timeout=180, check=False)
        print(f"  truncate issued (+{(time.time()-t_trunc)*1000:.0f} ms to return)")

        # Wait for the recall we are about to be the victim of.
        t_cb = None
        deadline = time.time() + 20
        while time.time() < deadline:
            if c.cb_calls:
                t_cb = time.time()
                break
            time.sleep(0.02)
        if t_cb:
            print(f"  CB_LAYOUTRECALL received +{(t_cb-t_trunc)*1000:.0f} ms "
                  f"and ANSWERED: {c.cb_calls[0]}")
        else:
            print("  !! no CB_LAYOUTRECALL arrived within 20s")

        # THE MEASUREMENT: from here the client has been recalled and has
        # no layout. How long until it can get one again, and what does it
        # see meanwhile?
        timeline, t0 = [], time.time()
        first_ok = None
        nonlocal_c = [c]
        while time.time() - t0 < poll_secs:
            try:
                _, s, _ = nonlocal_c[0].layoutget(fh, timeout=15)
            except (ConnectionError, TimeoutError, OSError) as e:
                # A dropped connection during a long park is not a result.
                # Reconnect and keep measuring, but RECORD it — silently
                # papering over it would turn "the server hung up on a
                # parked client" into a gap in the timeline that reads
                # like nothing happened.
                dt = round((time.time() - t0) * 1000)
                timeline.append((dt, f"RECONNECT({type(e).__name__})"))
                try:
                    nonlocal_c[0].close(destroy=False)
                except Exception:
                    pass
                nc = sc.Nfs41Client(host, port, verbose=verbose)
                nc.exchange_id(); nc.create_session(); nc.reclaim_complete()
                st2, fh2 = nc.lookup_path([pvc, fname])
                nonlocal_c[0] = nc
                if fh2:
                    fh = fh2
                continue
            dt = round((time.time() - t0) * 1000)
            if not timeline or timeline[-1][1] != s:
                timeline.append((dt, s))
            if s == sc.NFS4_OK:
                first_ok = dt
                break
            time.sleep(0.25)
        c = nonlocal_c[0]

        def label(v):
            return v if isinstance(v, str) else sc.errname(v)
        print("  LAYOUTGET timeline after recall (ms, status):")
        for dt, s in timeline:
            print(f"    +{dt:>6} ms  {label(s)}")
        if first_ok is not None:
            print(f"  ==> recovered a layout after {first_ok} ms")
        else:
            print(f"  ==> NEVER recovered a layout within {poll_secs}s "
                  f"(last: {label(timeline[-1][1]) if timeline else 'n/a'})")
        return dict(arm=arm, recall_ms=round((t_cb - t_trunc) * 1000) if t_cb else None,
                    timeline=[(d, label(s)) for d, s in timeline],
                    recovered_ms=first_ok)
    finally:
        if stopped:
            kubectl("scale", "statefulset", "flint-pnfs-ds", "-n", NS,
                    "--replicas=2", check=False)
            print("  DS fleet restored to 2 (waiting for Ready)")
            deadline = time.time() + 240
            while time.time() < deadline:
                pods = ds_pods()
                if len(pods) >= 2 and all(p[1] == "Running" for p in pods):
                    break
                time.sleep(3)
        c.close()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=12049)
    ap.add_argument("--pvc", required=True)
    ap.add_argument("--writer-pod", default="r1-writer")
    ap.add_argument("--arm", choices=["baseline", "ds-down", "both"], default="both")
    ap.add_argument("--poll-secs", type=float, default=120)
    ap.add_argument("--down-secs", type=float, default=60,
                    help="how long to keep the DS away before restoring it")
    ap.add_argument("--verbose", action="store_true")
    a = ap.parse_args()

    arms = ["baseline", "ds-down"] if a.arm == "both" else [a.arm]
    results = []
    for i, arm in enumerate(arms):
        results.append(run_arm(arm, a.host, a.port, a.pvc, f"r1-{arm}.bin",
                               a.writer_pod, a.poll_secs, a.verbose, a.down_secs))

    print(f"\n{'='*66}\nSUMMARY\n{'='*66}")
    for r in results:
        print(json.dumps(r))
    return 0


if __name__ == "__main__":
    sys.exit(main())
