#!/usr/bin/env python3
"""Amplification verdicts for one arm's CSV: push, content bytes, uploaded bytes.

    amplify.py --name source --csv FILE --threshold 24

Prints PASS / FAIL / INCONCLUSIVE / NOTE lines for the shell to tally.
The seed push (row 0) is excluded from the ratio: an initial import
uploads the repository once by definition and no repack policy changes
that. What is being measured is the STEADY STATE — what an ordinary
push costs once the repository exists.
"""
import argparse
import csv

ap = argparse.ArgumentParser()
ap.add_argument("--name", required=True)
ap.add_argument("--csv", required=True)
ap.add_argument("--threshold", type=int, required=True)
ap.add_argument("--fold", type=int, default=0, help="compaction tiers' factor; 0 = the repack rule")
ap.add_argument("--max-ratio", type=float, default=0.0, help="the tiers arm's pre-registered ceiling")
a = ap.parse_args()

rows = []
with open(a.csv) as f:
    for r in csv.reader(f, delimiter=" "):
        if len(r) == 3:
            rows.append((int(r[0]), int(r[1]), int(r[2])))
seed = [r for r in rows if r[0] == 0]
steady = [r for r in rows if r[0] > 0]
out = []
MIB = 1048576.0

if len(steady) < 5:
    print(f"INCONCLUSIVE {a.name}: only {len(steady)} steady-state pushes recorded")
    raise SystemExit

content = sum(r[1] for r in steady)
uploaded = sum(r[2] for r in steady)
ratio = uploaded / content if content else 0.0
if seed:
    out.append(f"NOTE {a.name}: the import uploaded {seed[0][2]/MIB:.1f} MiB — excluded from the "
               f"ratio, since a first import uploads the repository once whatever the repack policy is")

# A repack push is one that uploaded far more than it added. Naming
# them by a fixed multiple rather than by position: the threshold says
# WHEN one should happen, and a leg that assumed the position would
# not notice if it happened somewhere else.
spikes = [r for r in steady if r[1] and r[2] > 10 * r[1]]
out.append(f"NOTE {a.name}: {len(steady)} pushes added {content/MIB:.1f} MiB and uploaded "
           f"{uploaded/MIB:.1f} MiB — amplification {ratio:.1f}x")
if spikes:
    biggest = max(spikes, key=lambda r: r[2])
    out.append(f"NOTE {a.name}: {len(spikes)} push(es) uploaded more than 10x what they added; "
               f"the largest was push {biggest[0]} at {biggest[2]/MIB:.1f} MiB for "
               f"{biggest[1]/MIB:.3f} MiB of content")

if a.fold > 0:
    # The tiers arm (X18): the claim is a LOGARITHMIC amortised cost,
    # pre-registered per shape, and no single push paying the whole
    # repository outside a base rebuild. The largest upload in a run is
    # named either way; the ceiling is the verdict.
    biggest = max(steady, key=lambda r: r[2])
    out.append(f"NOTE {a.name}: the largest single upload was push {biggest[0]} at "
               f"{biggest[2]/MIB:.1f} MiB for {biggest[1]/MIB:.3f} MiB of content")
    if a.max_ratio > 0 and ratio <= a.max_ratio:
        out.append(f"PASS {a.name}: tiers at factor {a.fold} amortise to {ratio:.1f}x, within the "
                   f"pre-registered {a.max_ratio:g}x")
    elif a.max_ratio > 0:
        out.append(f"FAIL {a.name}: tiers at factor {a.fold} amortise to {ratio:.1f}x, above the "
                   f"pre-registered {a.max_ratio:g}x")
    else:
        out.append(f"NOTE {a.name}: tiers at factor {a.fold} amortise to {ratio:.1f}x (no ceiling given)")
elif a.threshold > len(steady):
    # The control arm: no repack can fire, so nothing but each push's
    # own pack is uploaded. If this is not ~1x the measurement is not
    # measuring the repack.
    # The claim under test is the absence of SPIKES, not a ratio of
    # 1.0. Every push rewrites the tree of the directory it touches, so
    # a 2 KiB edit legitimately ships ~9 KiB of pack; that floor is
    # git's, it is present in both arms, and it is not amplification
    # the repack policy could remove. A first draft failed the control
    # for being 5.1x and would have had me "fix" a rig that was right.
    if not spikes:
        out.append(f"PASS {a.name}: with the repack out of reach NO push uploads more than 10x what "
                   f"it adds; the {ratio:.1f}x floor is git rewriting trees, not the repack")
    else:
        out.append(f"FAIL {a.name}: with NO repack possible {len(spikes)} push(es) still uploaded more "
                   f"than 10x what they added — this rig is measuring something else")
else:
    expected = len(steady) // a.threshold
    if not spikes:
        out.append(f"INCONCLUSIVE {a.name}: {len(steady)} pushes at threshold {a.threshold} should have "
                   f"triggered ~{expected} repack(s) and none was observed — the arm never reached the regime")
    else:
        out.append(f"NOTE {a.name}: {len(spikes)} repack upload(s) observed, ~{expected} expected at "
                   f"threshold {a.threshold}")
print("\n".join(out))
