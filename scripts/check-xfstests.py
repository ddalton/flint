#!/usr/bin/env python3
"""Gate the xfstests differential (leg C10).

Reads two ./check transcripts — flint and the knfsd control — and gates
on the DIFFERENTIAL, not on either raw count.

Why not the raw count. Over NFS a large slice of generic/ is either
inapplicable (block-device, XFS-specific, O_DIRECT alignment) or fails
against any NFS server because the protocol cannot express what the test
asserts. knfsd is the reference implementation; whatever it fails is NFS
being NFS, and charging flint for it would make the number meaningless.

Three columns are reported, and the third is the one people forget:

  REGRESSIONS   fail on flint, pass on knfsd      -> evidence about flint
  COVERAGE LOSS notrun on flint, ran on knfsd     -> flint declined the
                                                     question; NOT a pass
  UNEARNED      fail on knfsd, pass on flint      -> usually flint being
                                                     more permissive, not
                                                     more correct. Printed
                                                     so it cannot be quoted
                                                     as a win unexamined.
  UNGUARDED     fail on flint, notrun on knfsd    -> the control DECLINED
                                                     the test and flint
                                                     plowed in. Almost
                                                     always flint
                                                     misreporting a
                                                     capability the test
                                                     guards on -- the
                                                     zero-space FSSTAT
                                                     answer defeats
                                                     _require_fs_space this
                                                     way. Counted
                                                     SEPARATELY because it
                                                     lands in neither of
                                                     the first two columns
                                                     and would otherwise
                                                     vanish.
"""
import json
import re
import sys

SECTIONS = ("Ran:", "Not run:", "Failures:")


def parse(path):
    """Return (ran, notrun, failed) as sets of test ids."""
    try:
        text = open(path, errors="replace").read()
    except OSError as e:
        print(f"FAIL: cannot read {path}: {e}")
        sys.exit(1)

    out = {k: set() for k in SECTIONS}
    current = None
    for line in text.splitlines():
        header = next((s for s in SECTIONS if line.startswith(s)), None)
        if header:
            current = header
            line = line[len(header):]
        elif current and not re.match(r"^\s+\S+/\S+", line):
            # Section ends at the first line that is not a continuation.
            current = None
        if current:
            out[current] |= set(re.findall(r"\b([a-z]+/\d+[a-z]?)\b", line))
    return out["Ran:"], out["Not run:"], out["Failures:"]


def main():
    if len(sys.argv) != 4:
        print("usage: check-xfstests.py <flint.txt> <knfsd.txt> <baseline.json>")
        sys.exit(2)
    flint_path, knfsd_path, baseline_path = sys.argv[1:4]

    f_ran, f_notrun, f_fail = parse(flint_path)
    k_ran, k_notrun, k_fail = parse(knfsd_path)

    # Anti-vacuity. A transcript that parsed to nothing is a broken run,
    # and "0 regressions" out of 0 tests must never read as a pass.
    if not f_ran:
        print("FAIL: the flint transcript lists no tests as Ran — the run died, it did not pass")
        sys.exit(1)
    if not k_ran:
        print("FAIL: the knfsd CONTROL transcript lists no tests as Ran. This is an "
              "INFRASTRUCTURE error: without a control arm the flint number cannot be "
              "attributed and must not be quoted.")
        sys.exit(1)

    f_pass = f_ran - f_fail - f_notrun
    k_pass = k_ran - k_fail - k_notrun

    regressions = sorted(f_fail & k_pass)
    coverage_loss = sorted(f_notrun & (k_ran - k_notrun))
    unearned = sorted(k_fail & f_pass)
    unguarded = sorted(f_fail & k_notrun)

    print(f"flint : ran={len(f_ran):4d} pass={len(f_pass):4d} fail={len(f_fail):3d} notrun={len(f_notrun):4d}")
    print(f"knfsd : ran={len(k_ran):4d} pass={len(k_pass):4d} fail={len(k_fail):3d} notrun={len(k_notrun):4d}")
    print()
    print(f"REGRESSIONS   (fail on flint, pass on knfsd) : {len(regressions)}")
    for t in regressions:
        print(f"    {t}")
    print(f"COVERAGE LOSS (notrun on flint, ran on knfsd): {len(coverage_loss)}")
    for t in coverage_loss:
        print(f"    {t}")
    print(f"UNEARNED      (fail on knfsd, pass on flint) : {len(unearned)}"
          "   <- inspect before quoting as a win")
    for t in unearned:
        print(f"    {t}")
    print(f"UNGUARDED     (fail on flint, notrun on knfsd): {len(unguarded)}"
          "   <- control declined; flint ran and failed")
    for t in unguarded:
        print(f"    {t}")

    try:
        baseline = json.load(open(baseline_path))
    except OSError:
        # NOT a pass. `tests/lima/xfstests-baseline.json` has never
        # existed (`git log -- 'tests/lima/xfstests*'` is empty), so this
        # arm was the only one that ever ran: a ~40-minute suite that
        # could not fail, printing a candidate baseline and returning 0.
        # Every anti-vacuity guard above it had never seen real input.
        #
        # The candidate is still printed — that is genuinely useful, and
        # it is how you record the first baseline — but recording it is
        # now a deliberate act rather than the default outcome.
        print(f"\nFAIL: no baseline at {baseline_path}, so nothing was gated.")
        print("      Record one from a run you have actually inspected:")
        print(json.dumps({"max_regressions": len(regressions),
                          "max_coverage_loss": len(coverage_loss),
                          "max_unguarded": len(unguarded),
                          "known_regressions": regressions,
                          "known_coverage_loss": coverage_loss,
                          "known_unguarded": unguarded}, indent=2))
        print(f"      ...into {baseline_path}, then re-run.")
        return 1

    rc = 0
    max_reg = baseline.get("max_regressions")
    if max_reg is not None and len(regressions) > max_reg:
        print(f"\nFAIL: {len(regressions)} regressions exceeds the floor of {max_reg}")
        new = sorted(set(regressions) - set(baseline.get("known_regressions", [])))
        if new:
            print("      new since the baseline: " + " ".join(new))
        rc = 1
    max_cov = baseline.get("max_coverage_loss")
    if max_cov is not None and len(coverage_loss) > max_cov:
        print(f"\nFAIL: {len(coverage_loss)} coverage losses exceeds the floor of {max_cov}")
        rc = 1
    max_ung = baseline.get("max_unguarded")
    if max_ung is not None and len(unguarded) > max_ung:
        print(f"\nFAIL: {len(unguarded)} unguarded failures exceeds the floor of {max_ung}")
        rc = 1
    if rc == 0:
        print(f"\nPASS: {len(regressions)} regressions (floor {max_reg}), "
              f"{len(coverage_loss)} coverage losses (floor {max_cov}), "
              f"{len(unguarded)} unguarded (floor {max_ung})")
    return rc


if __name__ == "__main__":
    sys.exit(main())
