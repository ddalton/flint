#!/usr/bin/env python3
"""Diff two pjdfstest transcripts ASSERTION BY ASSERTION, and say WHY.

    pjdfstest-delta.py <after.txt> <before.txt> <knfsd-control.txt> [--shape]

WHY THIS EXISTS. `pjdfstest-differential.sh` reports the flint-only count
by AREA (chown:182, rename:93, ...), and an area total invites the wrong
reading of a change. Arm C moved rename +30 and unlink +18 and that read
as "48 unexplained failures in rename and unlink", with `unlink ...
expected 0, got ENOENT` looking like files going missing. It was not: all
48 were the LINK refusal and its cascade, in four files. The area totals
could not show that; a per-assertion diff could.

An assertion is identified by (file, prove's test number), taken from the
Test Summary Report — the same source `pjdfstest-differential.sh` parses,
and NOT from the streaming `not ok` lines, which renumber under -j. The
streaming lines are read only for the `tried '...'` TEXT of each failure.

THE CLASSIFIER IS NOT A JUDGEMENT, IT IS A LEDGER. It attributes a
failure to an earlier refusal in the SAME FILE, walking that file's
transcript in order: a `link SRC DST` refused with 524 means DST was
never created, so every later failure naming DST is downstream of it, and
so is any failure naming SRC (whose second name never existed, so nothing
the test did through that name reached the inode). Anything it cannot
attribute lands in RESIDUE and is printed in full — RESIDUE is the output
that matters, and a classifier that explains everything is a classifier to
distrust, not a clean bill of health.
"""
import re, sys, collections

def parse_summary(p):
    """(file -> set of failed assertion numbers), from prove's summary."""
    out, cur, started, in_failed = {}, None, False, False
    for line in open(p, errors="replace").read().splitlines():
        if "Test Summary Report" in line: started = True; continue
        if not started: continue
        m = re.match(r"^(\S+\.t)\s+\(Wstat", line)
        if m: cur = m.group(1); out.setdefault(cur, set()); in_failed = False; continue
        m = re.search(r"Failed tests?:\s+(.*)$", line)
        if m and cur: out[cur] |= _expand(m.group(1)); in_failed = True; continue
        # `TODO passed:` and friends have continuation lines that look
        # exactly like a failed-test list. Stop at the next label.
        if re.match(r"^\s+\w[\w ]*:", line): in_failed = False; continue
        if cur and in_failed and re.match(r"^\s+[\d,\s-]+$", line): out[cur] |= _expand(line)
    return out

def _expand(s):
    g = set()
    for part in s.replace(" ", "").split(","):
        if not part: continue
        if "-" in part:
            a, _, b = part.partition("-")
            if a.isdigit() and b.isdigit(): g |= set(range(int(a), int(b) + 1))
        elif part.isdigit(): g.add(int(part))
    return g

def parse_inline(p):
    """(file, n) -> the `tried '...'` text, from the streaming section."""
    out, cur = {}, None
    for line in open(p, errors="replace").read().splitlines():
        if "Test Summary Report" in line: break
        m = re.match(r"^(\S+\.t)\s", line)
        if m: cur = m.group(1); continue
        m = re.match(r"^not ok (\d+)(?: - (.*))?$", line)
        if m and cur: out[(cur, int(m.group(1)))] = (m.group(2) or "").strip()
    return out

LINK = re.compile(r"tried '(?:-[ug] \d+ )*link (\S+) (\S+)', expected (\S+), got (\S+)")

def classify(inline):
    per = collections.defaultdict(list)
    for (t, n), txt in inline.items(): per[t].append((n, txt))
    cause = {}
    for t, rows in per.items():
        rows.sort()
        phantom, tainted, prev = set(), set(), None
        seen = lambda s, txt: any(x in txt for x in s)
        for n, txt in rows:
            c = None
            m = LINK.match(txt)
            if m:
                src, dst, _, got = m.groups()
                if got == "524":
                    phantom.add(dst); tainted.add(src); c = "A: LINK itself refused (524)"
                elif seen(phantom, src): phantom.add(dst); c = "B: names a file the refused link never created"
            if c is None and seen(phantom, txt): c = "B: names a file the refused link never created"
            if c is None and seen(tainted, txt): c = "C: the surviving name, whose second link never existed"
            if c is None and txt == "" and prev and prev[0] in "ABCD": c = "D: continuation of the assertion above"
            cause[(t, n)] = c or "E: RESIDUE"
            prev = cause[(t, n)]
    return cause

DENY = {"EACCES","EPERM","EROFS","EISDIR","ENOTDIR","EEXIST","ENOENT","ELOOP",
        "ENAMETOOLONG","EINVAL","EXDEV","ENOTEMPTY","EFBIG","ETXTBSY","EMLINK"}

def shape(only, inline):
    """What KIND of wrong each failure is. `PERMITTED what must be refused`
    is the one to watch: a suite where permitting everything scores points
    is exactly why this reads per-assertion rather than by score."""
    out, ex = collections.Counter(), {}
    for t, ns in only.items():
        for n in ns:
            txt = inline.get((t, n), "")
            m = re.search(r"expected (\S+), got (\S+)$", txt)
            if not m: k = "(continuation line, no text of its own)"
            else:
                e, g = m.groups()
                if e in DENY and g == "0": k = f"flint PERMITTED what must be refused ({e} owed)"
                elif e == "0" and g in DENY: k = f"flint REFUSED what must succeed (got {g})"
                elif e in DENY and g in DENY: k = f"refused with the wrong error ({e} owed, {g} given)"
                else: k = "attribute/value mismatch"
            out[k] += 1; ex.setdefault(k, f"{t} #{n}: {txt[:160]}")
    return out, ex

def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    if len(args) != 3:
        print(__doc__); return 2
    after, before, ctl = args
    a, b, k = parse_summary(after), parse_summary(before), parse_summary(ctl)
    ia, ib = parse_inline(after), parse_inline(before)
    if not k:
        print("VOID: the control transcript parsed to nothing — without a"
              "\n      trustworthy control no number here means anything.")
        return 1
    only = lambda f: {t: f[t] - k.get(t, set()) for t in f if f[t] - k.get(t, set())}
    oa, ob = only(a), only(b)
    files = set(oa) | set(ob)
    new  = sorted([(t, n) for t in files for n in (oa.get(t, set()) - ob.get(t, set()))])
    gone = sorted([(t, n) for t in files for n in (ob.get(t, set()) - oa.get(t, set()))])
    na, nb = sum(map(len, oa.values())), sum(map(len, ob.values()))
    print(f"flint-only  after : {na}\nflint-only before : {nb}\n"
          f"NEW  : {len(new)}\nGONE : {len(gone)}\nnet  : {na - nb:+d}\n")

    if "--shape" in sys.argv:
        for label, o, i in (("after", oa, ia), ("before", ob, ib)):
            c, ex = shape(o, i)
            print(f"── shape of the {label} arm ──")
            for kk, v in c.most_common(): print(f"  {v:4d}  {kk}\n        e.g. {ex[kk]}")
            print()
        return 0

    if new:
        print("── NEW failures by file ──")
        for t, c in collections.Counter(t for t, _ in new).most_common(): print(f"  {c:4d}  {t}")
        cause = classify(ia)
        print("\n── why ──")
        for kk, v in sorted(collections.Counter(cause.get(x, "E: RESIDUE") for x in new).items()):
            print(f"  {v:4d}  {kk}")
        resid = [x for x in new if cause.get(x, "E: RESIDUE").startswith("E")]
        print(f"\nRESIDUE — NOT attributable to an earlier refusal: {len(resid)}")
        for t, n in resid: print(f"   {t} #{n}: {ia.get((t, n), '<no text>')[:200]}")
    if gone:
        print(f"\n── the {len(gone)} that failed BEFORE and pass AFTER ──")
        print("   (a higher pass count is not automatically better — check each)")
        for t, n in gone[:40]: print(f"   {t} #{n}: before: {ib.get((t, n), '<no text>')[:160]}")
    return 0

if __name__ == "__main__":
    sys.exit(main())
