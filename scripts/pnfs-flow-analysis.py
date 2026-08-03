#!/usr/bin/env python3
"""Per-flow drain-chain classification for pnfs-flow-classification.sh.

Input: OUT dir with flow-client-N.log / flow-ds-N.log (1 Hz raw `ss -tinH`
dumps bracketed by `T <epoch>` lines), reads.txt (n mibps t0 t1),
softirq-N-{pre,post}.txt.

The discrimination matrix (2026-08-03 research pass):
  delivery_rate >> achieved, flows idle  -> app-limited (flint DS send)
  sndbuf_limited dominant                -> DS socket configuration
  cwnd small + retransmits               -> fabric / path property
  rwnd_limited or client softirq pegged  -> client receive path

Parsing is deliberately off-node and noisy-input-tolerant; any flow whose
counters never move across a read is REPORTED as dead, not silently
dropped (the eight instrument bugs rule)."""

import re
import sys
import statistics as st
from collections import defaultdict

OUT = sys.argv[1]

FLOW_RE = re.compile(r"^\S")          # ss -H: each socket line starts unindented
KV = {
    "bytes_acked": re.compile(r"bytes_acked:(\d+)"),
    "bytes_received": re.compile(r"bytes_received:(\d+)"),
    "cwnd": re.compile(r"\bcwnd:(\d+)"),
    "rtt": re.compile(r"\brtt:([\d.]+)/"),
    "busy": re.compile(r"busy:(\d+)ms"),
    "rwnd_limited": re.compile(r"rwnd_limited:(\d+)ms"),
    "sndbuf_limited": re.compile(r"sndbuf_limited:(\d+)ms"),
    "retrans_total": re.compile(r"\bretrans:\d+/(\d+)"),
    "delivery_rate": re.compile(r"delivery_rate\s+([\d.]+)([kMG])bps"),
    "pacing_rate": re.compile(r"pacing_rate\s+([\d.]+)([kMG])bps"),
}
MULT = {"k": 1e3, "M": 1e6, "G": 1e9}
ADDR = re.compile(r"(\d+\.\d+\.\d+\.\d+:\d+)\s+(\d+\.\d+\.\d+\.\d+:\d+)")


def rate(m):
    return float(m.group(1)) * MULT[m.group(2)] if m else None


def parse_dump(path):
    """-> list of (ts, {flowkey: {field: value}}) samples."""
    samples = []
    cur_ts, cur = None, {}
    try:
        lines = open(path).read().splitlines()
    except FileNotFoundError:
        return []
    i = 0
    while i < len(lines):
        ln = lines[i]
        if ln.startswith("T "):
            if cur_ts is not None:
                samples.append((cur_ts, cur))
            cur_ts, cur = float(ln.split()[1]), {}
        else:
            am = ADDR.search(ln)
            if am:
                key = f"{am.group(1)}->{am.group(2)}"
                # tcp_info may be on the same line (-H) or the next
                blob = ln
                if i + 1 < len(lines) and not lines[i + 1].startswith("T ") \
                        and not ADDR.search(lines[i + 1]):
                    blob += " " + lines[i + 1]
                    i += 1
                d = {}
                for f, rx in KV.items():
                    m = rx.search(blob)
                    if m:
                        d[f] = rate(m) if f.endswith("_rate") else float(m.group(1))
                if d:
                    cur.setdefault(key, {}).update(d)
        i += 1
    if cur_ts is not None:
        samples.append((cur_ts, cur))
    return samples


def window(samples, t0, t1):
    return [(ts, f) for ts, f in samples if t0 - 1.5 <= ts <= t1 + 1.5]


def flow_deltas(samples, field):
    """Per-flow (last - first) for a monotone counter within samples."""
    firsts, lasts = {}, {}
    for ts, flows in samples:
        for k, d in flows.items():
            if field in d:
                firsts.setdefault(k, d[field])
                lasts[k] = d[field]
    return {k: lasts[k] - firsts[k] for k in lasts if k in firsts}


reads = []
for ln in open(f"{OUT}/reads.txt"):
    n, mibps, t0, t1 = ln.split()
    if n == "0":
        continue  # self-test read
    reads.append((n, int(mibps), int(t0) / 1e9, int(t1) / 1e9))

if not reads:
    print("no measured reads")
    sys.exit(1)

print("=" * 74)
print(" PER-FLOW SPLIT (client bytes_received per read) — the never-run check")
print("=" * 74)
per_read_flows = []
agg_split = []
for n, mibps, t0, t1 in reads:
    cs = parse_dump(f"{OUT}/flow-client-{n}.log")
    deltas = {k: v for k, v in flow_deltas(cs, "bytes_received").items() if v > 1 << 24}
    wall = t1 - t0
    per_read_flows.append((n, mibps, deltas, wall))
    if deltas:
        rates = sorted((v / wall / 2**20 for v in deltas.values()), reverse=True)
        total = sum(rates)
        split = max(rates) / max(min(rates), 0.01) if len(rates) > 1 else 1.0
        agg_split.append(split)
        print(f"  read #{n:>2}: {mibps:>5} MiB/s | {len(rates)} active flows | "
              f"per-flow MiB/s: {', '.join(f'{r:.0f}' for r in rates)} "
              f"| max/min {split:.2f}x")
    else:
        print(f"  read #{n:>2}: {mibps:>5} MiB/s | NO ACTIVE FLOWS SEEN — sampler dead?")

n_flows = [len(d) for _, _, d, _ in per_read_flows if d]
if n_flows:
    print(f"\n  flows carrying the read: {sorted(set(n_flows))} "
          f"(equal-split max/min median {st.median(agg_split):.2f}x)"
          if agg_split else "")

print()
print("=" * 74)
print(" LIMITER CLASSIFICATION (DS-side tcp_info deltas per read)")
print("=" * 74)
verdict_acc = defaultdict(float)
for n, mibps, t0, t1 in reads:
    ds = parse_dump(f"{OUT}/flow-ds-{n}.log")
    dwin = window(ds, t0, t1)
    if not dwin:
        print(f"  read #{n:>2}: no DS samples in window")
        continue
    acked = {k: v for k, v in flow_deltas(dwin, "bytes_acked").items() if v > 1 << 24}
    busy = flow_deltas(dwin, "busy")
    rwnd = flow_deltas(dwin, "rwnd_limited")
    sndb = flow_deltas(dwin, "sndbuf_limited")
    wall_ms = (t1 - t0) * 1000
    # last-sample instantaneous fields over the flows that carried data
    cwnds, drates, rtts, retr = [], [], [], 0
    for ts, flows in dwin[-3:]:
        for k in acked:
            d = flows.get(k, {})
            if "cwnd" in d:
                cwnds.append(d["cwnd"])
            if "delivery_rate" in d:
                drates.append(d["delivery_rate"])
            if "rtt" in d:
                rtts.append(d["rtt"])
    r0 = flow_deltas(dwin, "retrans_total")
    retr = sum(v for k, v in r0.items() if k in acked)
    tb = sum(busy.get(k, 0) for k in acked)
    tr = sum(rwnd.get(k, 0) for k in acked)
    tsb = sum(sndb.get(k, 0) for k in acked)
    flow_ms = wall_ms * max(len(acked), 1)
    achieved_bps = sum(acked.values()) * 8 / (t1 - t0)
    dr = st.median(drates) * len(acked) if drates else 0
    print(f"  read #{n:>2}: {len(acked)} flows | busy {tb / flow_ms * 100:5.1f}% | "
          f"rwnd-lim {tr / flow_ms * 100:5.1f}% | sndbuf-lim {tsb / flow_ms * 100:5.1f}% | "
          f"cwnd med {st.median(cwnds):.0f} | retrans +{retr:.0f} | "
          f"delivery {dr / 8 / 2**20:.0f} vs achieved {achieved_bps / 8 / 2**20:.0f} MiB/s"
          if cwnds else f"  read #{n:>2}: {len(acked)} flows (no instantaneous fields)")
    verdict_acc["rwnd"] += tr / flow_ms
    verdict_acc["sndbuf"] += tsb / flow_ms
    verdict_acc["busy"] += tb / flow_ms
    verdict_acc["retrans"] += retr
    if dr and achieved_bps:
        verdict_acc["dr_ratio"] += dr / achieved_bps
        verdict_acc["dr_n"] += 1

print()
print("=" * 74)
print(" CLIENT SOFTIRQ (per-cpu, worst core during each read)")
print("=" * 74)


def softirq(path):
    cpus, tick = {}, 100
    for ln in open(path):
        f = ln.split()
        if f and f[0] == "STAT":
            cpus[f[1]] = int(f[2])
        elif f and f[0] == "TICK":
            tick = int(f[1])
    return cpus, tick


worst_cores = []
for n, mibps, t0, t1 in reads:
    try:
        pre, tick = softirq(f"{OUT}/softirq-{n}-pre.txt")
        post, _ = softirq(f"{OUT}/softirq-{n}-post.txt")
    except FileNotFoundError:
        continue
    wall = t1 - t0
    pct = {c: (post[c] - pre[c]) / tick / wall * 100 for c in post if c in pre}
    if pct:
        top = sorted(pct.items(), key=lambda kv: -kv[1])[:3]
        worst_cores.append(top[0][1])
        print(f"  read #{n:>2}: top softirq cores: "
              + ", ".join(f"{c}={p:.0f}%" for c, p in top))

print()
print("=" * 74)
print(" VERDICT")
print("=" * 74)
nr = len(reads)
rwnd_share = verdict_acc["rwnd"] / nr * 100
sndbuf_share = verdict_acc["sndbuf"] / nr * 100
busy_share = verdict_acc["busy"] / nr * 100
dr_ratio = (verdict_acc["dr_ratio"] / verdict_acc["dr_n"]) if verdict_acc.get("dr_n") else 0
softirq_peg = max(worst_cores) if worst_cores else 0
print(f"  flow-time shares: busy {busy_share:.1f}%  rwnd-limited {rwnd_share:.1f}%  "
      f"sndbuf-limited {sndbuf_share:.1f}%")
print(f"  delivery/achieved ratio: {dr_ratio:.2f}   worst client softirq core: {softirq_peg:.0f}%")
print(f"  total DS retransmits across reads: {verdict_acc['retrans']:.0f}")
print()
findings = []
if rwnd_share > 30 or softirq_peg > 80:
    findings.append("CLIENT RECEIVE PATH: rwnd-limited/softirq dominate — the top research "
                    "candidate is CONFIRMED as the proximate limiter")
if sndbuf_share > 30:
    findings.append("DS SOCKET: sndbuf-limited dominates — DS socket buffer sizing is the lever")
if dr_ratio > 2 and busy_share < 40:
    findings.append("APP-LIMITED: the path could deliver far more than the DS offers — "
                    "the limiter is UPSTREAM in flint's send path after all")
if verdict_acc["retrans"] > 1000:
    findings.append("FABRIC: heavy retransmission — path loss is back in play")
if not findings:
    findings.append("NO SINGLE DOMINANT LIMITER at these shares — see per-read rows; "
                    "the constraint may be RTT/BDP shaped (check cwnd vs rtt) or split")
for f in findings:
    print(f"  -> {f}")
