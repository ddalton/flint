#!/usr/bin/env python3
"""Gate the full pynfs NFSv4.1 run.

`test-nfs-protocol` used to end its pynfs invocation in `|| true`, so the
target exited 0 whatever happened — including the two outcomes that
matter most: the suite collapsing to zero passes, and the suite never
starting at all (a server that refused to boot, a VM that was down, a
--maketree that failed). Both look identical to success when the exit
status is discarded.

Removing `|| true` on its own is not the answer either. pynfs exits
non-zero while ANY test fails, and this suite has known, deliberately
deferred failures, so the target would be permanently red — which gates
nothing, and gets ignored exactly like a permanently green one.

So the oracle is here rather than in the exit status, the same shape
`check-pynfs42.py` already uses: the run must have HAPPENED (results
present, parseable, and of plausible size), and the pass count must not
fall below a recorded floor. The floor lives in a committed baseline so
a regression fails loudly instead of being recorded in a JSON nobody
reads.

Usage:
    check-pynfs.py <results.json> [baseline.json]

Exit 0 = gate passed. Exit 1 = regression or the run did not happen.
"""
import json
import os
import sys

# The suite is ~262 testcases. Anything far below that means the run did
# not really execute — the single most important thing to catch, because
# it is the failure that most resembles success.
MIN_TESTCASES = 200

# ...but a SKIP is not evidence that anything was tested. The recorded
# floor was 171 PASS / 0 FAIL / 91 SKIP; a newer pynfs clone gives
# 175/23/68, and under the old rules that PASSED — 175 clears the 171
# floor, 266 clears MIN_TESTCASES, and the 23 codes that moved SKIP->FAIL
# were never looked at, because FAIL was counted, printed, and never
# gated on. Both holes are closed below: failures have a ceiling, and the
# anti-collapse count is over EXECUTED cases only.
#
# `min_executed` defaults to `min_pass` when the baseline does not record
# it, which is exactly right for a 171/0/91 baseline and needs no
# re-baselining to take effect.
DEFAULT_MAX_FAIL = 0


def classify(tc):
    if "skipped" in tc:
        return "SKIP"
    if "failure" in tc:
        return "FAIL"
    if "error" in tc:
        return "ERROR"
    return "PASS"


def main():
    if len(sys.argv) < 2:
        print("usage: check-pynfs.py <results.json> [baseline.json]")
        return 1
    results_path = sys.argv[1]
    baseline_path = sys.argv[2] if len(sys.argv) > 2 else os.path.join(
        os.path.dirname(os.path.abspath(__file__)),
        "..", "tests", "lima", "pynfs-baseline.json",
    )

    if not os.path.exists(results_path):
        print(f"FAIL: no results at {results_path} — the run did not produce output.")
        print("      This is the collapse case: pynfs never ran, or died before writing JSON.")
        return 1
    try:
        with open(results_path) as fh:
            data = json.load(fh)
    except (json.JSONDecodeError, OSError) as e:
        print(f"FAIL: results at {results_path} are unreadable ({e}).")
        return 1

    cases = data.get("testcase", [])
    counts = {"PASS": 0, "FAIL": 0, "SKIP": 0, "ERROR": 0}
    for tc in cases:
        counts[classify(tc)] += 1
    total = len(cases)

    print(f"pynfs 4.1: {total} testcases — "
          f"PASS={counts['PASS']} FAIL={counts['FAIL']} "
          f"SKIP={counts['SKIP']} ERROR={counts['ERROR']}")

    executed = counts["PASS"] + counts["FAIL"] + counts["ERROR"]

    if total < MIN_TESTCASES:
        print(f"\nFAIL: only {total} testcases ran (expected >= {MIN_TESTCASES}).")
        print("      The suite did not really execute. Check that the server booted,")
        print("      that --maketree succeeded, and that the VM is up.")
        return 1

    floor = None
    baseline_doc = {}
    if os.path.exists(baseline_path):
        try:
            with open(baseline_path) as fh:
                baseline_doc = json.load(fh)
            floor = baseline_doc.get("min_pass")
        except (json.JSONDecodeError, OSError) as e:
            print(f"FAIL: baseline at {baseline_path} is unreadable ({e}).")
            return 1

    if floor is None:
        print(f"\nFAIL: no pass floor recorded at {baseline_path}.")
        print("      Record one from a run you have actually inspected:")
        print(f'        echo \'{{"min_pass": {counts["PASS"]}}}\' > {baseline_path}')
        print("      Refusing to pass on an unpinned baseline — an unrecorded floor")
        print("      is how a conformance number becomes folklore.")
        return 1

    if counts["PASS"] < floor:
        print(f"\nFAIL: {counts['PASS']} passes is below the recorded floor of {floor}.")
        print("      Either a regression landed, or the floor needs re-basing after")
        print("      a deliberate change — do not lower it without saying why.")
        return 1

    # A SKIP tests nothing, so it cannot be evidence the run happened.
    # Without this, swapping in a clone that skips more and fails more
    # passes the gate on the strength of the skips.
    min_executed = baseline_doc.get("min_executed", floor)
    if executed < min_executed:
        print(f"\nFAIL: only {executed} testcases actually EXECUTED "
              f"(PASS+FAIL+ERROR), below the floor of {min_executed}.")
        print("      SKIPs are not evidence of a run. A clone whose codes moved")
        print("      SKIP->FAIL, or a rig that cannot reach a feature, lands here.")
        return 1

    # Failures are gated, not merely printed. This is the one that was
    # computed and thrown away.
    max_fail = baseline_doc.get("max_fail", DEFAULT_MAX_FAIL)
    bad = counts["FAIL"] + counts["ERROR"]
    if bad > max_fail:
        print(f"\nFAIL: {counts['FAIL']} failures + {counts['ERROR']} errors "
              f"= {bad}, above the recorded ceiling of {max_fail}.")
        print("      A conformance gate that counts only passes cannot see a")
        print("      regression that turns a SKIP into a FAIL. If these are")
        print("      known and accepted, record them:")
        print(f'        {{"min_pass": {floor}, "max_fail": {bad}}}')
        return 1

    print(f"\nPYNFS 4.1 GATE PASSED ({counts['PASS']} passes >= floor {floor}, "
          f"{executed} executed >= {min_executed}, {bad} failures <= {max_fail})")
    if counts["PASS"] > floor:
        print(f"NOTE: {counts['PASS'] - floor} more passes than the floor — "
              f"raise it to {counts['PASS']} to lock the gain in.")
    return 0


sys.exit(main())
