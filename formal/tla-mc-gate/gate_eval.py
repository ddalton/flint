#!/usr/bin/env python3
"""Re-judge gate_par.py output by sweep.py's exact rule: a mutation_run passes
when it violates and want matches the name (re.search) OR appears in the
violation line (want in out). Every other check is gate_par.py's."""
import json, re, sys, collections
c = collections.Counter(); bad = []
for l in open(sys.argv[1]):
    r = json.loads(l)
    if r.get("skip"):
        c["skipped"] += 1; continue
    res = r["result"]
    if res != "OK":
        probs = [p for p in res[len("DIFF: "):].split("; ")]
        keep = []
        for p in probs:
            m = re.match(r"(interp|gen) verdict (\S+) want (.*)$", p)
            if m and r["kind"] == "mutation_run":
                v, want = m.group(2), r["want"]
                lines = [f"{k} {v} is violated" for k in ("Invariant", "Action property", "Temporal property", "Property")]
                if v != "None" and (re.search(want, v) or any(want in s for s in lines)):
                    continue
            keep.append(p)
        res = "OK" if not keep else "DIFF: " + "; ".join(keep)
    c["OK" if res == "OK" else "DIFF"] += 1
    if res != "OK":
        bad.append(f"{r['dir']}/{r['cfg']} {res}")
    g = r.get("gen", {})
    if g.get("step_props_compiled"):
        c["entries with compiled step props"] += 1
print(dict(c))
print("\n".join(bad))
