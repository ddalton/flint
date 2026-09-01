#!/usr/bin/env python3
"""Stripe-width gate — the discriminator that root-caused F-OCIAB-1/2.

Reads a DEBUG-level MDS log and checks, per LAYOUTGET, that the stripe
pattern flint put on the wire matches the file's pinned placement:

    nfl_first_stripe_index  ==  file_id % stripe_width
    stripe_width            ==  the same value for every grant of a file

The 1.43.0 defect: `encode_file_layout_striped` derived the stripe width
from `segments.len()`, which for a BOUNDED LAYOUTGET was the number of
stripe units the request spanned. A 4 KiB read therefore advertised
width 1 and rotation `file_id % 1 == 0`, pointing the client at slot 0
while the bytes lived on slot `file_id % N` — an absent stripe file, read
as a sparse hole, i.e. ZEROS RETURNED WITH NFS4_OK.

    usage:  stripe-width-gate.py <mds-debug.log[.gz]> [--expect-width N]

Exit 0 = PASS, 1 = FAIL, 2 = INCONCLUSIVE.

**INCONCLUSIVE is not PASS.** The defect only manifests on bounded
grants, so a log with no bounded grant in it cannot exonerate anything —
that is the anti-vacuity leg, and it is why this exits 2 rather than 0
when the workload never exercised the path. A GREEN run must show
bounded grants PRESENT and all at full width.

The MDS must be running at debug level; at INFO none of these lines
exist and the gate is blind (it says so rather than passing).
"""
import gzip
import re
import sys
from collections import Counter, defaultdict

ANSI = re.compile(r"\x1b\[[0-9;]*m")
RE_REQ = re.compile(r"LAYOUTGET: offset=(\d+), length=(\d+), iomode=(\w+)")
RE_WIDTH = re.compile(r"Number of DSes in stripe: (\d+)")
RE_FSI = re.compile(r"first_stripe_index: (\d+)")
RE_FID = re.compile(r"file_id ([0-9a-f]{16})")
RE_OPEN = re.compile(r"Encoding STRIPED FILE layout")
RE_CLOSE = re.compile(r"Encoded STRIPED FILE layout")


def load(path):
    op = gzip.open if path.endswith(".gz") else open
    with op(path, "rt", errors="replace") as fh:
        return [ANSI.sub("", l) for l in fh]


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    if not args:
        print(__doc__)
        return 2
    expect = None
    for a in sys.argv[1:]:
        if a.startswith("--expect-width"):
            expect = int(a.split("=", 1)[1]) if "=" in a else None

    lines = load(args[0])

    # ── Pairing must be interleaving-safe. ───────────────────────────
    # The MDS is concurrent and tracing writes each line independently,
    # so under load two encode blocks braid together *within the same
    # microsecond*. A naive forward scan then pairs one grant's
    # first_stripe_index with another's file_id and invents rotation
    # mismatches: an earlier version of this gate reported 3 "failures"
    # in 526 grants on a run that was in fact clean. A gate that cries
    # wolf under concurrency is exactly as useless as one that passes
    # everything, and concurrency is when this defect matters most.
    #
    # So: a block counts as PAIRABLE only when it runs from its opening
    # marker to its closing marker with no second opening marker in
    # between. Braided blocks are reported as unpairable, never as
    # failures. The WIDTH check does not need pairing at all — the
    # width is on one line by itself — so it keeps full coverage.
    widths_all = [int(m.group(1)) for m in
                  (RE_WIDTH.search(l) for l in lines) if m]

    grants, unpairable, req = [], 0, None
    open_i = None
    for i, l in enumerate(lines):
        m = RE_REQ.search(l)
        if m:
            req = (int(m.group(1)), int(m.group(2)), m.group(3))
        if RE_OPEN.search(l):
            if open_i is not None:
                unpairable += 1      # previous block never closed cleanly
            open_i = i
            continue
        if RE_CLOSE.search(l) and open_i is not None:
            block = lines[open_i:i + 1]
            w = next((RE_WIDTH.search(b) for b in block if RE_WIDTH.search(b)), None)
            f = next((RE_FSI.search(b) for b in block if RE_FSI.search(b)), None)
            d = RE_FID.search(l)
            if w and f and d:
                grants.append((int(w.group(1)), int(f.group(1)),
                               int(d.group(1), 16), d.group(1), req))
            else:
                unpairable += 1
            open_i = None

    if not grants:
        print("INCONCLUSIVE: no striped-layout encode lines found.")
        print("  The MDS must run at DEBUG. At INFO these lines do not exist,")
        print("  so their absence says nothing about the stripe width.")
        return 2

    widths = Counter(widths_all)
    modal = expect or widths.most_common(1)[0][0]

    # 1. Rotation must equal file_id % width, per grant.
    bad_rot = [g for g in grants if g[1] != g[2] % g[0]]
    # 2. Width must be the same for every grant of a given file.
    per_file = defaultdict(set)
    for w, _, _, fid, _ in grants:
        per_file[fid].add(w)
    split = {f: ws for f, ws in per_file.items() if len(ws) > 1}
    # 3. Every grant must be at the pinned width.
    narrow = [w for w in widths_all if w != modal]
    # 4. Anti-vacuity: the bounded path must actually have been exercised.
    bounded = [g for g in grants if g[4] and g[4][1] != (1 << 64) - 1]

    print(f"encode blocks   : {len(widths_all)} ({len(grants)} pairable, "
          f"{unpairable} braided by concurrency — reported, never failed)")
    print(f"widths seen     : {dict(widths)}  (pinned width taken as {modal})")
    print(f"bounded grants  : {len(bounded)} (the path that carried the defect)")
    print()

    fail = False
    if narrow:
        fail = True
        print(f"✗ {len(narrow)} encode(s) NOT at the pinned width {modal}: "
              f"widths {sorted(set(narrow))}")
        for w, fsi, fidv, fid, req in [g for g in grants if g[0] != modal][:8]:
            shape = f"offset={req[0]} length={req[1]} iomode={req[2]}" if req else "?"
            print(f"    file_id {fid}  width={w} fsi={fsi}  "
                  f"(file_id%{modal}={fidv % modal})  <- {shape}")
    if bad_rot:
        fail = True
        print(f"✗ {len(bad_rot)} grant(s) whose rotation != file_id % width — the "
              f"client is sent to a slot that does not hold the bytes:")
        for w, fsi, fidv, fid, _ in bad_rot[:8]:
            print(f"    file_id {fid}  advertised fsi={fsi}, correct={fidv % w}")
    if split:
        fail = True
        print(f"✗ {len(split)} file(s) given CONTRADICTORY stripe maps across grants:")
        for fid, ws in list(split.items())[:8]:
            print(f"    file_id {fid}  widths {sorted(ws)}")

    if fail:
        print("\nFAIL — this is the F-OCIAB-1/2 signature: a client reading under "
              "one of these layouts gets zeros with NFS4_OK.")
        return 1

    if not bounded:
        print("INCONCLUSIVE: every grant was a whole-file (length=u64::MAX) grant.")
        print("  The defect only appears on BOUNDED grants, so this log cannot")
        print("  distinguish a fixed server from one that was never asked.")
        print("  Re-run with a workload that issues small reads (a registry push,")
        print("  or any first-touch read of a small file).")
        return 2

    print(f"PASS — all {len(grants)} grants at width {modal} with "
          f"fsi == file_id % {modal}, including {len(bounded)} bounded grant(s).")
    return 0


if __name__ == "__main__":
    sys.exit(main())
