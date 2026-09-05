#!/usr/bin/env python3
"""Verdicts for a latency-leg CSV: rtt,rep,arm,pos,ms with arm A the
shipped fan-out and arm B the control (fanout 1, what the code did
before).

    analyze.py --what push --csv FILE --saved N [--null-ms M]

`--saved` is the number of round trips arm A is PREDICTED to save per
operation, stated before the numbers are read. Prints PASS / FAIL /
INCONCLUSIVE / NOTE lines for the shell to tally. The rules:

  * at RTT 0 the arms must be indistinguishable — the knob alone must
    move nothing, or the difference at RTT>0 is not latency's;
  * at each RTT>0, A must be faster in >=80% of interleaved pairs AND
    by at least half the prediction (PASS); by a quarter to a half is
    INCONCLUSIVE; under a quarter — or not faster in enough pairs — is
    FAIL, because a saving that small is the effect being ABSENT (the
    pre-fix binary, with no knob to differ on, "saved" 23 ms of 200);
  * across RTTs the saving must scale with the RTT — a saving that does
    not grow with the round trip is not a round-trip saving.
"""
import argparse
import csv
import math
import statistics as st
from collections import defaultdict

ap = argparse.ArgumentParser()
ap.add_argument("--what", required=True)
ap.add_argument("--csv", required=True)
ap.add_argument("--saved", type=float, required=True)
ap.add_argument("--null-ms", type=float, default=60.0)
a = ap.parse_args()

by = defaultdict(lambda: defaultdict(dict))
with open(a.csv) as f:
    for row in csv.reader(f):
        if not row or row[0].startswith("#"):
            continue
        rtt, rep, arm, _pos, ms = row
        by[int(rtt)][arm][int(rep)] = float(ms)


def q(v, p):
    v = sorted(v)
    k = (len(v) - 1) * p
    lo, hi = math.floor(k), math.ceil(k)
    return v[lo] + (v[hi] - v[lo]) * (k - lo)


def iqr(v):
    return q(v, .75) - q(v, .25)


out, deltas, meds = [], {}, {}
for r in sorted(by):
    A, B = by[r].get("A", {}), by[r].get("B", {})
    reps = sorted(set(A) & set(B))
    if len(reps) < 3:
        out.append(f"INCONCLUSIVE RTT {r} ms: only {len(reps)} paired {a.what} samples")
        continue
    va, vb = [A[k] for k in reps], [B[k] for k in reps]
    mA, mB = st.median(va), st.median(vb)
    d = mB - mA
    wins = sum(1 for k in reps if B[k] > A[k])
    meds[r], deltas[r] = (mA, mB), d
    out.append(f"NOTE RTT {r:>3} ms: fanout A {mA:7.0f} ms   fanout 1 {mB:7.0f} ms   "
               f"delta {d:+7.0f} ms   fanout 1 slower in {wins}/{len(reps)}   "
               f"IQR {iqr(va):.0f}/{iqr(vb):.0f} ms")
    need = math.ceil(0.8 * len(reps))
    if r == 0:
        if abs(d) <= a.null_ms:
            out.append(f"PASS at RTT 0 the arms are indistinguishable (delta {d:+.0f} ms): "
                       f"the knob alone moves nothing")
        else:
            out.append(f"INCONCLUSIVE at RTT 0 the arms differ by {d:+.0f} ms — something other "
                       f"than latency separates them")
        continue
    pred = a.saved * r
    if wins >= need and d >= 0.5 * pred:
        out.append(f"PASS at RTT {r} ms the fan-out saves {d:.0f} ms per {a.what} "
                   f"(predicted {pred:.0f} = {a.saved:g} round trips)")
    elif wins >= need and d >= 0.25 * pred:
        out.append(f"INCONCLUSIVE at RTT {r} ms the fan-out saves {d:.0f} ms per {a.what}, "
                   f"under half the predicted {pred:.0f}")
    else:
        out.append(f"FAIL at RTT {r} ms the fan-out is not faster: delta {d:+.0f} ms, "
                   f"fanout 1 slower in only {wins}/{len(reps)} (predicted {pred:.0f})")

nz = [r for r in sorted(deltas) if r > 0]
if len(nz) >= 2:
    def fit(ys):
        xs = nz
        mx, my = sum(xs) / len(xs), sum(ys) / len(ys)
        k = sum((x - mx) * (y - my) for x, y in zip(xs, ys)) / sum((x - mx) ** 2 for x in xs)
        return k, my - k * mx
    kA, cA = fit([meds[r][0] for r in nz])
    kB, cB = fit([meds[r][1] for r in nz])
    out.append(f"NOTE fitted over RTT {nz} ms: fanout A = {kA:.1f} round trips + {cA:.0f} ms, "
               f"fanout 1 = {kB:.1f} round trips + {cB:.0f} ms; saved {kB - kA:.1f} "
               f"(predicted {a.saved:g})")
    lo, hi = nz[0], nz[-1]
    if deltas[lo] > 0:
        ratio, want = deltas[hi] / deltas[lo], hi / lo
        if 0.6 * want <= ratio <= 1.4 * want:
            out.append(f"PASS the saving scales with the round trip ({deltas[lo]:.0f} ms at {lo} "
                       f"-> {deltas[hi]:.0f} ms at {hi}, x{ratio:.1f} for x{want:.0f}): "
                       f"it is latency, not CPU")
        else:
            out.append(f"INCONCLUSIVE the saving does not scale with the round trip "
                       f"({deltas[lo]:.0f} ms at {lo} -> {deltas[hi]:.0f} ms at {hi}, "
                       f"x{ratio:.1f} for x{want:.0f})")
    else:
        out.append(f"FAIL no saving at RTT {lo} ms to scale from")
print("\n".join(out))
