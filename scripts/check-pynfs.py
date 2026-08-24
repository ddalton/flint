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

    if total < MIN_TESTCASES:
        print(f"\nFAIL: only {total} testcases ran (expected >= {MIN_TESTCASES}).")
        print("      The suite did not really execute. Check that the server booted,")
        print("      that --maketree succeeded, and that the VM is up.")
        return 1

    floor = None
    if os.path.exists(baseline_path):
        try:
            with open(baseline_path) as fh:
                floor = json.load(fh).get("min_pass")
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

    print(f"\nPYNFS 4.1 GATE PASSED ({counts['PASS']} passes >= floor {floor})")
    if counts["PASS"] > floor:
        print(f"NOTE: {counts['PASS'] - floor} more passes than the floor — "
              f"raise it to {counts['PASS']} to lock the gain in.")
    return 0


sys.exit(main())
