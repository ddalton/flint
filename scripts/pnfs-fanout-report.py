#!/usr/bin/env python3
"""Turn one pnfs-fanout-diag.sh capture into the three numbers that matter.

    ./scripts/pnfs-fanout-report.py /tmp/pnfs-diag/<label>

1. FAN-OUT — bytes each data server actually put on the wire during the fio
   window, and its share. A client that reads 1040 MiB/s from five DSes and
   one that reads 423 MiB/s from one look identical from the client; only
   this column tells them apart.
2. RPC ACCOUNTING — the client's own split of where a READ's time went:
   backlog (queued on the client, no slot) vs RTT (server) vs execute. If
   RTT is flat and backlog explodes, the server is fine and the client is
   throttling itself; the reverse means the fleet.
3. CPU — the client's user/sys/softirq/iowait split over the same window.

Everything is a delta over the fio window specifically, not over the pod's
lifetime: `apk add` and pod teardown are outside it.
"""
import json
import os
import re
import sys

MIB = 1024.0 * 1024.0


def read(path):
    try:
        with open(path) as fh:
            return fh.read()
    except OSError:
        return ""


def fio_window(text):
    """(start, end, read_MiBs, write_MiBs) from the fio pod's log."""
    m0 = re.search(r"FIO_START=(\d+)", text)
    m1 = re.search(r"FIO_END=(\d+)", text)
    start = int(m0.group(1)) if m0 else None
    end = int(m1.group(1)) if m1 else None
    rd = wr = None
    brace = text.find("{")
    if brace >= 0:
        try:
            # fio's JSON is bracketed by our two echo lines.
            blob = text[brace:text.rfind("}") + 1]
            j = json.loads(blob)
            job = j["jobs"][0]
            rd = job["read"]["bw_bytes"] / MIB
            wr = job["write"]["bw_bytes"] / MIB
        except Exception:
            pass
    return start, end, rd, wr


def nic_delta(path, start, end):
    """(rx_bytes, tx_bytes, samples_in_window) over [start, end]."""
    rows = []
    for line in read(path).splitlines():
        f = line.split()
        if len(f) == 3 and all(x.isdigit() for x in f):
            rows.append(tuple(int(x) for x in f))
    if not rows:
        return None
    win = [r for r in rows if start is None or (start <= r[0] <= end)]
    if len(win) < 2:
        return None
    return win[-1][1] - win[0][1], win[-1][2] - win[0][2], len(win), win[-1][0] - win[0][0]


def parse_mountstats(text):
    """{device: {'ops': {NAME: [ints]}, 'xprt': [[fields]], 'bytes': [ints]}}"""
    out, cur = {}, None
    for line in text.splitlines():
        s = line.strip()
        if s.startswith("device "):
            m = re.match(r"device (\S+) mounted on (\S+) with fstype (\S+)", s)
            if m and m.group(3).startswith("nfs"):
                cur = f"{m.group(1)} -> {m.group(2)}"
                out[cur] = {"ops": {}, "xprt": [], "bytes": []}
            else:
                cur = None
            continue
        if cur is None:
            continue
        if s.startswith("xprt:"):
            f = s.split()[1:]
            out[cur]["xprt"].append([int(x) for x in f[1:] if x.lstrip("-").isdigit()])
        elif s.startswith("bytes:"):
            out[cur]["bytes"] = [int(x) for x in s.split()[1:]]
        else:
            m = re.match(r"^([A-Z_0-9]+):\s+((?:\d+\s*)+)$", s)
            if m:
                out[cur]["ops"][m.group(1)] = [int(x) for x in m.group(2).split()]
    return out


def split_snapshot(text):
    """(epoch, /proc/stat text, mountstats text)"""
    parts = text.split("---MOUNTSTATS---")
    head = parts[0].split("---STAT---")
    ts = None
    for tok in head[0].split():
        if tok.isdigit():
            ts = int(tok)
            break
    return ts, (head[1] if len(head) > 1 else ""), (parts[1] if len(parts) > 1 else "")


def cpu_delta(a, b):
    def first(text):
        for line in text.splitlines():
            if line.startswith("cpu "):
                return [int(x) for x in line.split()[1:]]
        return None
    x, y = first(a), first(b)
    if not x or not y:
        return None
    d = [q - p for p, q in zip(x, y)]
    tot = sum(d) or 1
    names = ["user", "nice", "sys", "idle", "iowait", "irq", "softirq", "steal"]
    return {n: 100.0 * v / tot for n, v in zip(names, d[:len(names)])}


def main(outdir):
    label = os.path.basename(outdir.rstrip("/"))
    start, end, rd, wr = fio_window(read(os.path.join(outdir, "fio.json")))
    dur = (end - start) if (start and end) else None

    print(f"\n══ {label} ══")
    if rd is None and wr is None:
        print("  ! no fio result parsed — check fio.json")
    else:
        print(f"  fio: read {rd:8.1f} MiB/s   write {wr:8.1f} MiB/s"
              f"   window {dur}s")

    # ── 1. fan-out ──────────────────────────────────────────────────────
    nics = {}
    for fn in sorted(os.listdir(outdir)):
        if fn.startswith("nic-") and fn.endswith(".txt"):
            node = fn[4:-4]
            d = nic_delta(os.path.join(outdir, fn), start, end)
            if d:
                nics[node] = d
    if nics:
        placed = read(os.path.join(outdir, "placed-on.txt")).strip()
        ds = {k: v for k, v in nics.items() if k != placed}
        tot_tx = sum(v[1] for v in ds.values()) or 1
        print("\n  data-server egress over the fio window")
        print("    node                                    tx MiB    MiB/s   share")
        for node, (rx, tx, n, span) in sorted(ds.items(), key=lambda kv: -kv[1][1]):
            rate = tx / MIB / (span or 1)
            print(f"    {node:<38} {tx/MIB:8.0f} {rate:8.1f}  {100.0*tx/tot_tx:5.1f}%")
        shares = sorted((v[1] / tot_tx for v in ds.values()), reverse=True)
        if shares:
            print(f"    -> {len(shares)} servers, top share {100*shares[0]:.1f}%, "
                  f"bottom {100*shares[-1]:.1f}%")
            evenness = shares[-1] / shares[0] if shares[0] else 0
            verdict = ("EVEN" if evenness > 0.7 else
                       "SKEWED" if evenness > 0.15 else "COLLAPSED")
            print(f"    -> fan-out: {verdict} (min/max = {evenness:.2f})")
        if placed in nics:
            rx, tx, n, span = nics[placed]
            print(f"    client {placed}: rx {rx/MIB:.0f} MiB "
                  f"({rx/MIB/(span or 1):.1f} MiB/s), tx {tx/MIB:.0f} MiB")

    # ── 2. client RPC accounting ────────────────────────────────────────
    ts_a, stat_a, ms_a = split_snapshot(read(os.path.join(outdir, "client-before.txt")))
    ts_b, stat_b, ms_b = split_snapshot(read(os.path.join(outdir, "client-after.txt")))
    A, B = parse_mountstats(ms_a), parse_mountstats(ms_b)
    for dev in sorted(B):
        if dev not in A:
            continue
        rows = []
        for op in ("READ", "WRITE", "GETATTR", "LAYOUTGET", "COMMIT"):
            a = A[dev]["ops"].get(op)
            b = B[dev]["ops"].get(op)
            if not a or not b or len(b) < 8:
                continue
            d = [q - p for p, q in zip(a, b)]
            if d[0] <= 0:
                continue
            rows.append((op, d))
        if not rows:
            continue
        # Per-op fields: ops trans timeouts bytes_sent bytes_recv
        #                queue_ms rtt_ms execute_ms [errors]
        print(f"\n  RPC accounting  {dev}")
        print("    op            ops  KiB out   KiB in  backlog ms   RTT ms"
              "  execute ms")
        stray_write = 0
        for op, d in rows:
            ops = d[0]
            if op in ("WRITE", "COMMIT"):
                stray_write += ops
            print(f"    {op:<10} {ops:7d} {d[3]/1024.0/ops:8.1f} "
                  f"{d[4]/1024.0/ops:8.1f} {d[5]/ops:11.3f} {d[6]/ops:8.3f} "
                  f"{d[7]/ops:11.3f}")
        # THE BELT. A read run that records WRITE/COMMIT RPCs is measuring
        # fio's own file layout, not reads — see the job-name note in
        # pnfs-fanout-diag.sh. Left unchecked it produced a confident
        # 3.4x "regression" that did not exist.
        if rd and wr is not None and rd > wr and stray_write > 100:
            print(f"    *** {stray_write} WRITE/COMMIT RPCs during a READ run —"
                  f" THIS NUMBER IS VOID (file layout inside the window) ***")
        # xprt: port bind connect_count connect_time idle_time sends recvs
        #       bad_xids req_u bklog_u max_slots sending_u pending_u
        xa, xb = A[dev]["xprt"], B[dev]["xprt"]
        if xa and xb and len(xa) == len(xb):
            print(f"    transports: {len(xb)}")
            for i, (p, q) in enumerate(zip(xa, xb)):
                if len(q) < 13:
                    continue
                d = [y - x for x, y in zip(p, q)]
                sends = d[5] or 1
                print(f"      xprt[{i}] sends {d[5]:8d} recvs {d[6]:8d} "
                      f"avg backlog depth {d[9]/sends:7.2f} "
                      f"max_slots {q[10]:4d} avg pending {d[12]/sends:6.2f}")

    # ── 3. client CPU ───────────────────────────────────────────────────
    c = cpu_delta(stat_a, stat_b)
    if c:
        busy = 100.0 - c["idle"]
        print(f"\n  client CPU over the bracket (includes ~{(ts_b-ts_a-(dur or 0)) if ts_a and ts_b else '?'}s idle)")
        print("    busy {:.1f}%  user {:.1f}  sys {:.1f}  softirq {:.1f} "
              " iowait {:.1f}  steal {:.1f}".format(
                  busy, c["user"], c["sys"], c["softirq"], c["iowait"], c["steal"]))
    print()


if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else ".")
