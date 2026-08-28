#!/usr/bin/env python3
"""Gate the flint-vs-knfsd performance differential.

    check-perf.py <measurements.json> [baseline.json]

Exit 0 = gate passed. Exit 1 = regression, or the run did not really
happen.

WHAT IS GATED, AND WHY IT IS A RATIO. The rig is a 2 vCPU / 2 GiB VM on
a laptop that is usually compiling something, and a prior campaign
measured the same rig drifting ~2x within a single session. An absolute
MiB/s from it is not a quantity you can compare to last week's. The
flint/knfsd ratio, measured in the same session on the same kernel and
the same disk with the arms interleaved, is — the rig divides out.

So the baseline records ratios, and a regression is the ratio falling.
The number that would look impressive in a release note is deliberately
not the number that gates.
"""
import json
import os
import statistics
import sys

# Below this, the run did not happen. Never gate on the absence of
# failures: a generator that died produces no bad measurements at all.
MIN_REPS = 3
# Rig noise on a contended 2-vCPU VM is large. A band tighter than this
# produces a gate that cries wolf and gets disabled, which is worse than
# no gate. Tune it DOWN on a quiet dedicated rig, never up.
DEFAULT_TOLERANCE = 0.25


def rate(rec):
    """Work per second. None when the record proves nothing."""
    ns = rec.get("ns", 0)
    if ns <= 0:
        return None
    # A dd that wrote no bytes, or a create loop whose files did not
    # appear, finishes fast and would otherwise score superbly. The
    # amount of work is part of the measurement, not decoration.
    work = rec.get("bytes", rec.get("ops", 0))
    if not work:
        return None
    return work / (ns / 1e9)


def collect(path):
    by = {}
    bad = 0
    with open(path) as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            try:
                rec = json.loads(line)
            except json.JSONDecodeError:
                bad += 1
                continue
            r = rate(rec)
            if r is None:
                bad += 1
                continue
            by.setdefault((rec["dim"], rec["arm"]), []).append(r)
    return by, bad


def main():
    if len(sys.argv) < 2:
        print(__doc__)
        return 1
    meas_path = sys.argv[1]
    baseline_path = sys.argv[2] if len(sys.argv) > 2 else "tests/lima/perf-baseline.json"

    if not os.path.exists(meas_path):
        print(f"FAIL: no measurements at {meas_path} — the run did not produce output.")
        return 1

    by, bad = collect(meas_path)
    if bad:
        print(f"note: {bad} record(s) discarded as proving nothing (zero work or zero time)")

    dims = sorted({d for (d, _) in by})
    if not dims:
        print("FAIL: no usable measurements at all — the run died, it did not pass.")
        return 1

    ratios = {}
    print(f"{'dimension':<10} {'flint':>14} {'knfsd':>14} {'ratio':>8}  reps")
    for dim in dims:
        f = by.get((dim, "flint"), [])
        k = by.get((dim, "knfsd"), [])
        n = min(len(f), len(k))
        if n < MIN_REPS:
            print(f"FAIL: {dim} has only {n} paired rep(s) (need >= {MIN_REPS}).")
            print("      A short run is VOID, not a pass — the arms must be paired,")
            print("      because an unpaired rep has no control to divide by.")
            return 1
        mf, mk = statistics.median(f), statistics.median(k)
        if mk <= 0:
            print(f"FAIL: {dim} control arm measured zero — knfsd did not run.")
            return 1
        unit = "ops/s" if dim == "meta" else "MiB/s"
        scale = 1.0 if dim == "meta" else 1024 * 1024
        ratios[dim] = mf / mk
        print(f"{dim:<10} {mf/scale:>10.1f} {unit:<4} {mk/scale:>10.1f} {unit:<4} "
              f"{ratios[dim]:>8.3f}  {n}")

    if not os.path.exists(baseline_path):
        # NOT a pass. The xfstests checker used to return 0 here and the
        # baseline it asked for was never written, so a 40-minute suite
        # ran for months unable to fail. Same trap, refused the same way.
        print(f"\nFAIL: no baseline at {baseline_path}, so nothing was gated.")
        print("      Record one from a run you have actually inspected, on a QUIET rig:")
        print(json.dumps({"tolerance": DEFAULT_TOLERANCE,
                          "ratios": {d: round(r, 3) for d, r in ratios.items()}}, indent=2))
        return 1

    with open(baseline_path) as fh:
        base = json.load(fh)
    tol = base.get("tolerance", DEFAULT_TOLERANCE)
    want = base.get("ratios", {})
    if not want:
        print(f"FAIL: baseline at {baseline_path} records no ratios.")
        return 1

    rc = 0
    print()
    for dim, floor in sorted(want.items()):
        got = ratios.get(dim)
        if got is None:
            print(f"FAIL: {dim} is in the baseline but was not measured — "
                  f"a dimension that stops running must not pass by absence.")
            rc = 1
            continue
        limit = floor * (1 - tol)
        if got < limit:
            print(f"FAIL: {dim} ratio {got:.3f} is below {limit:.3f} "
                  f"(baseline {floor:.3f} - {int(tol*100)}%).")
            print(f"      flint lost ground against the control. The rig cannot explain")
            print(f"      this: knfsd was measured in the same session, interleaved.")
            rc = 1
        else:
            print(f"ok:   {dim} ratio {got:.3f} >= {limit:.3f}")

    if rc == 0:
        print("\nPERF GATE PASSED")
        for dim, floor in sorted(want.items()):
            got = ratios[dim]
            if got > floor * (1 + tol):
                print(f"NOTE: {dim} is {got:.3f} vs baseline {floor:.3f} — if that is a "
                      f"real gain, re-baseline to lock it in.")
    return rc


sys.exit(main())
