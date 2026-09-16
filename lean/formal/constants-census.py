#!/usr/bin/env python3
"""Every constant the module declares must be known to EVERY cfg generator.

    constants-census.py [--selftest]

WHY. `LeanSubtree.tla` has two cfg producers, and they are not near each
other: `gen-cfgs.sh` writes the 112 gate cfgs, and `trace/ndjson2tla.py`
writes the cfg for a replayed trace. Adding a constant to the module and
to only one of them leaves the other's cfgs missing a value — TLC then
refuses the run with

    Error: The constant parameter X is not assigned a value by the
    configuration file.

which is a hard failure, but only in the gate you did not run. That is
exactly what happened twice: `ClaimMintsEpoch`/`ClaimStampsEpoch`
(2026-09-15) broke `trace-check.sh` — which runs in CI — and it went
unnoticed because `check.sh` was green, and `CollectorOff` broke it again
the same day. Both were found by a replay days later, not by the gate.

So the census is the gate's, not a habit's: `check.sh` runs this first.
"""
import re
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent

# Declared in the module but supplied per-run by every generator through
# its own machinery rather than a literal in the constant table.
SUPPLIED_SPECIALLY = {
    "Paths",     # the path set, sized by NPaths/FreeLast (gen-cfgs) or the trace
    "FreePaths",
    "Writers",   # a sequence; TLC cfgs cannot hold a sequence literal, so
                 # both generators emit `Writers <- SixWriters` and friends
}


def declared(module: Path):
    """The CONSTANTS block: names up to the first definition or VARIABLES."""
    text = module.read_text()
    after = text.split("\nCONSTANTS", 1)[1]
    out = []
    for line in after.splitlines():
        st = line.strip()
        if not st or st.startswith("\\*"):
            continue
        # A definition, an ASSUME or the VARIABLES block ends the list.
        if st.startswith(("VARIABLES", "VARIABLE", "ASSUME", "----", "====")) or "==" in st:
            break
        m = re.match(r"([A-Za-z][A-Za-z0-9_]*)\s*,?(?:\s*\\\*.*)?$", st)
        if m:
            out.append(m.group(1))
    return out


def main():
    module = HERE / "LeanSubtree.tla"
    names = [n for n in declared(module) if n not in SUPPLIED_SPECIALLY]
    gen = (HERE / "gen-cfgs.sh").read_text()
    # gen-cfgs.sh carries each constant twice: in KEYS and as a default.
    keys = set(re.findall(r"[A-Za-z][A-Za-z0-9_]*", gen.split("KEYS=", 1)[1].split('"')[1]))
    # `local c_MaxGen=4 c_MaxSeq=6 ...` — several per line, only the first
    # carrying `local`, so the distinctive `c_` prefix is what to match.
    defaults = set(re.findall(r"\bc_([A-Za-z0-9_]+)=", gen))
    trace = (HERE / "trace" / "ndjson2tla.py").read_text()

    bad = []
    for n in names:
        where = []
        if n not in keys:
            where.append("gen-cfgs.sh KEYS")
        if n not in defaults:
            where.append("gen-cfgs.sh default (local c_%s=)" % n)
        if f'"{n}"' not in trace:
            where.append("trace/ndjson2tla.py")
        if where:
            bad.append((n, where))

    if "--selftest" in sys.argv:
        # The control: a constant nothing supplies must be caught. If this
        # passes while the census passes, the census is not reading the
        # module at all.
        fake = "ConstantNoGeneratorKnows"
        supplied = fake in keys and fake in defaults and f'"{fake}"' in trace
        if supplied:
            print("selftest FAILED: the probe name is somehow supplied", file=sys.stderr)
            return 1
        print("selftest ok: an unsupplied constant would be reported "
              f"({len(names)} real constants read from {module.name})")
        return 0

    if bad:
        print("FAIL: a constant is declared but not supplied by every cfg generator:",
              file=sys.stderr)
        for n, where in bad:
            print(f"  {n}: missing from {', '.join(where)}", file=sys.stderr)
        print("  (TLC refuses such a run: 'The constant parameter X is not "
              "assigned a value by the configuration file')", file=sys.stderr)
        return 1
    print(f"constants census ok: {len(names)} constants, all known to "
          f"gen-cfgs.sh and trace/ndjson2tla.py")
    return 0


if __name__ == "__main__":
    sys.exit(main())
