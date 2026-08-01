#!/usr/bin/env python3
"""Gate the NFSv4.2 pynfs results by NAMED CODE.

Why named codes and not an aggregate count: for 25 consecutive archived
runs the aggregate said "91 skipped" and nobody looked at WHICH. All four
4.2 tests were in there, skipped because testserver.py's --minorversion
defaults to 1 — so the suite reported a clean bill of health for
operations it never exercised. An aggregate gate cannot distinguish
"passed" from "never ran".

Hence: SKIP is a FAILURE here. A test that stops running is the exact
regression this file exists to catch.

The list is deliberately short. ALLOC1-3 and COPY5 are the ENTIRE 4.2
surface this pynfs has. COPY1..COPY4 appear in no artifact and in no
version of the suite present here; naming them would make the gate fail
for the wrong reason.
"""
import json
import sys

REQUIRED = {
    "ALLOC1": "st_sparse.testAllocateSupported",
    "ALLOC2": "st_sparse.testAllocateStateidZero",
    "ALLOC3": "st_sparse.testAllocateStateidOne",
    "COPY5": "st_copy.testZeroLengthCopy",
}


def main() -> int:
    path = sys.argv[1] if len(sys.argv) > 1 else "/tmp/flint-pynfs42-results.json"
    try:
        with open(path) as fh:
            data = json.load(fh)
    except FileNotFoundError:
        print(f"FAIL: {path} not found — the 4.2 run did not produce results")
        return 1

    seen = {}
    for tc in data.get("testcase", []):
        code = (tc.get("code") or "").upper()
        if code not in REQUIRED:
            continue
        if "skipped" in tc:
            seen[code] = "SKIP"
        elif "failure" in tc:
            seen[code] = "FAIL"
        elif "error" in tc:
            seen[code] = "ERROR"
        else:
            seen[code] = "PASS"

    bad = []
    for code, name in sorted(REQUIRED.items()):
        status = seen.get(code, "ABSENT")
        print(f"  {code:8s} {status:7s} {name}")
        if status != "PASS":
            bad.append((code, status))

    if bad:
        print()
        for code, status in bad:
            if status == "SKIP":
                print(
                    f"FAIL: {code} was SKIPPED. A skip here almost always means the "
                    f"run lost --minorversion=2 — the suite defaults to 1 and then "
                    f"silently omits every 4.2 test."
                )
            elif status == "ABSENT":
                print(f"FAIL: {code} did not appear in the results at all.")
            else:
                print(f"FAIL: {code} -> {status}")
        return 1

    print(f"\nPYNFS 4.2 GATE PASSED ({len(REQUIRED)}/{len(REQUIRED)} named codes PASS)")
    return 0


sys.exit(main())
