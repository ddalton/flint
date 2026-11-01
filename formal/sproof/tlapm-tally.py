#!/usr/bin/env python3
# tlapm-tally.py <tlapm.out>...: final status per obligation ID across tlapm --toolbox outputs (a ranged record's
# parts).  An id counts as proved/trivial if any backend reported so, else its last status.  The runners' summ.py
# keyed by source LOCATION and so merged two obligations that share one (135:3:135:20 in the stamp proof): off by one.
import sys, re
from collections import Counter
st, loc = {}, {}
for f in sys.argv[1:]:
    t = open(f).read()
    for bl in re.findall(r"@!!BEGIN\n(.*?)@!!END", t, re.S):
        d = dict(re.findall(r"@!!(\w+):(.*)", bl))
        if d.get("type") != "obligation" or "id" not in d:
            continue
        k = (f, d["id"]); loc[k] = d.get("loc", "?"); s = d.get("status")
        if st.get(k) not in ("proved", "trivial"):
            st[k] = s
c = Counter(st.values())
print("TALLY", dict(c), "total", sum(c.values()))
for k, s in sorted(st.items(), key=lambda kv: [int(n) for n in re.findall(r"\d+", loc[kv[0]])[:2]]):
    if s not in ("proved", "trivial"):
        print("NOT PROVED", loc[k], s, k[0])
