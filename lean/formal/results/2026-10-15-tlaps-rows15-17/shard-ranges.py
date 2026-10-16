#!/usr/bin/env python3
"""Cut a proof module into N line ranges for `tlapm --toolbox FIRST LAST`
(tlapm checks only the obligations inside the range).  Cuts fall only at a
top-level LEMMA/THEOREM, so no proof is split; ranges are balanced by lines.
Why: tlapm's own front end is single-threaded (EC2 2026-10-15: every tlapm at
100% of one core, zero Z3 running), so one module = one core.
Usage: shard-ranges.py MODULE.tla N   -> prints "FIRST LAST" per shard."""
import sys
L = open(sys.argv[1]).read().split('\n'); n = int(sys.argv[2])
starts = [i + 1 for i, l in enumerate(L) if l.startswith(('LEMMA ', 'THEOREM '))]
total = len(L)
cuts = [1]
for k in range(1, n):
    target = total * k / n
    c = min(starts, key=lambda s: abs(s - target))
    if c > cuts[-1]:
        cuts.append(c)
cuts.append(total + 1)
for a, b in zip(cuts, cuts[1:]):
    print(a, b - 1)
