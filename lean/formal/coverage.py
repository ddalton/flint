#!/usr/bin/env python3
"""Which invariant is checked in which world — and where it is NOT.

    coverage.py            write COVERAGE.md next to the cfgs
    coverage.py --check    exit 1 if COVERAGE.md is stale (for the gate)

WHY. The gate reports 110 green runs. That number says nothing about
whether the invariant that would catch a given failure was ever CHECKED in
a world that can produce it: an invariant is only as strong as the worlds
it runs in, and a cfg that omits it is a hole nobody sees. This reads every
cfg's INVARIANT lines and its constants, reads `check.sh` for which cfgs
are STRICT (must hold) and which are MUTATIONS (must fail), and crosses the
two: invariant x world-feature.

A feature is a property of the world that a safety argument depends on —
more than one writer, a crash, a restart, a UI write, the sentinel, two
paths, identical bytes, the writer-local queue, the per-barrier lease. A
cell that reads NO means: no strict run checks that invariant in a world
with that feature turned on. That is not automatically a defect (some
pairs are meaningless), but every NO should be either explained or closed,
and this file is where that argument is made rather than assumed.
"""
import json
import re
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent

# The features a safety argument leans on, and how to read each from a cfg.
FEATURES = [
    ("2+ writers", lambda c: c.get("Writers") in ("TwoWriters", "ThreeWriters",
                                                  "FourWriters", "FiveWriters", "SixWriters")
                             or c.get("Writers") is None),
    ("3 writers", lambda c: c.get("Writers") in ("ThreeWriters", "FourWriters",
                                                 "FiveWriters", "SixWriters")),
    ("2+ paths", lambda c: c.get("_paths", 0) >= 2),
    ("crash", lambda c: c.get("MaxCrashes", 0) > 0),
    ("restart", lambda c: c.get("MaxRestarts", 0) > 0),
    ("UI write", lambda c: c.get("MaxHitl", 0) > 0),
    ("sentinel", lambda c: c.get("SentinelEnabled") is True),
    ("same bytes", lambda c: c.get("MaxSameBytes", 0) > 0),
    ("barrier lease", lambda c: c.get("BarrierLease") is True),
    ("writer queue", lambda c: c.get("WriterQueue") is True),
    ("sync verb", lambda c: c.get("MaxSyncs", 0) > 0),
    ("declared removal", lambda c: c.get("MaxRemovals", 0) > 0),
    ("narrow (scoped)", lambda c: c.get("MaxNarrows", 0) > 0),
]

NUM = re.compile(r"^-?\d+$")


def parse_cfg(path: Path):
    """{constant: value}, plus _invariants, _paths, _spec."""
    out = {"_invariants": [], "_properties": [], "_spec": None, "_paths": 0}
    section = None
    for raw in path.read_text().splitlines():
        line = raw.strip()
        if not line or line.startswith("\\*"):
            continue
        head = line.split()[0]
        if head in ("CONSTANT", "CONSTANTS"):
            section = "const"
            continue
        if head in ("INVARIANT", "INVARIANTS"):
            rest = line.split(None, 1)
            if len(rest) > 1:
                out["_invariants"].append(rest[1].strip())
            section = "inv"
            continue
        if head in ("PROPERTY", "PROPERTIES"):
            rest = line.split(None, 1)
            if len(rest) > 1:
                out["_properties"].append(rest[1].strip())
            section = "prop"
            continue
        if head in ("SPECIFICATION", "INIT", "NEXT", "CHECK_DEADLOCK", "SYMMETRY", "VIEW"):
            if head == "SPECIFICATION":
                out["_spec"] = line.split(None, 1)[1].strip()
            section = None
            continue
        if section == "inv":
            out["_invariants"].append(line)
            continue
        if section == "prop":
            out["_properties"].append(line)
            continue
        if section == "const":
            if "<-" in line:
                k, v = line.split("<-", 1)
                out[k.strip()] = v.strip()
                continue
            if "=" not in line:
                continue
            k, v = line.split("=", 1)
            k, v = k.strip(), v.strip()
            if v == "TRUE":
                out[k] = True
            elif v == "FALSE":
                out[k] = False
            elif NUM.match(v):
                out[k] = int(v)
            else:
                out[k] = v
                if k == "Paths":
                    out["_paths"] = len([x for x in v.strip("{}").split(",") if x.strip()])
    return out


def read_check_sh():
    """({cfg: kind}, {invariant: [cfgs that must VIOLATE it]}).

    A mutation names the violation it requires. An invariant with no
    mutation has never been shown capable of failing in any world — it may
    be vacuously true, which is the failure mode a green gate hides."""
    kinds, refutes = {}, {}
    text = (HERE / "check.sh").read_text()
    # A run may continue over several lines (`\` at the end).
    joined, buf = [], ""
    for line in text.splitlines():
        buf += line.rstrip("\\")
        if line.rstrip().endswith("\\"):
            continue
        joined.append(buf.strip())
        buf = ""
    for s in joined:
        m = re.match(r"^(strict_run|mutation_run|probe_run)\b", s)
        if not m or s.startswith(("strict_run()", "mutation_run()", "probe_run()")):
            continue
        cfg = re.search(r"([A-Za-z0-9_]+\.cfg)", s)
        if not cfg:
            continue
        kind = m.group(1).replace("_run", "")
        kinds[cfg.group(1)] = kind
        if kind in ("mutation", "probe"):
            # the required-violation argument, the last quoted string
            for name in re.findall(r'(Inv_[A-Za-z]+|TypeOK|Probe[A-Za-z]+) is violated', s):
                refutes.setdefault(name, []).append(cfg.group(1))
    return kinds, refutes


def main():
    check = "--check" in sys.argv
    kinds, refutes = read_check_sh()
    cfgs = {}
    for p in sorted(HERE.glob("*.cfg")):
        c = parse_cfg(p)
        # Only the LeanSubtree worlds: the chunk modules have their own
        # constants and their own (small) invariant sets.
        if "BarrierLease" not in c and "InboxEnabled" not in c:
            continue
        c["_kind"] = kinds.get(p.name, "not in check.sh")
        cfgs[p.name] = c

    seen = {i for c in cfgs.values() for i in c["_invariants"]}
    invariants = sorted(i for i in seen if not i.startswith("Probe"))
    probes = sorted(i for i in seen if i.startswith("Probe"))
    strict = {n: c for n, c in cfgs.items() if c["_kind"] == "strict"}

    rows = []
    for inv in invariants:
        checked = [n for n, c in strict.items() if inv in c["_invariants"]]
        cells = []
        for fname, pred in FEATURES:
            n = sum(1 for name in checked if pred(cfgs[name]))
            cells.append(n)
        rows.append((inv, len(checked), cells))

    lines = [
        "# Which invariant is checked in which world",
        "",
        "Generated by `coverage.py`; do not edit. Regenerate after changing a cfg",
        "or `check.sh`, and read the zeros: a 0 means NO STRICT RUN checks that",
        "invariant in a world with that feature on. Some pairs are meaningless",
        "(a rename invariant in a world with no renames); the rest are holes, and",
        "`lean/SAFETY.md` is where each is either argued away or listed as open.",
        "",
        f"Worlds: {len(cfgs)} cfgs over `LeanSubtree.tla` — {len(strict)} strict "
        f"(must hold), {sum(1 for c in cfgs.values() if c['_kind'] == 'mutation')} mutations "
        f"(must fail), {sum(1 for c in cfgs.values() if c['_kind'] == 'probe')} probes, "
        f"{sum(1 for c in cfgs.values() if c['_kind'] == 'not in check.sh')} not run by the gate.",
        "",
        "| invariant | strict runs | refuted by | " + " | ".join(f for f, _ in FEATURES) + " |",
        "|---|---|---|" + "---|" * len(FEATURES),
    ]
    for inv, n, cells in rows:
        r = len(refutes.get(inv, []))
        lines.append(f"| `{inv}` | {n} | {r if r else '**0**'} | "
                     + " | ".join(str(x) if x else "**0**" for x in cells) + " |")
    lines += ["",
              "`refuted by` counts the MUTATION runs that require this invariant to be "
              "violated. A 0 there is the sharper hole: the invariant has never been shown "
              "capable of failing, so a green run over it may be vacuous.",
              "",
              f"Probe markers (must-fail reachability checks, not safety): {len(probes)} — "
              + ", ".join(f"`{p}`" for p in probes[:8]) + (", …" if len(probes) > 8 else "")]

    # The worlds themselves, so a reader can see what each covers.
    lines += ["", "## The strict worlds", "",
              "| cfg | invariants | writers | paths | features |", "|---|---|---|---|---|"]
    for name in sorted(strict):
        c = cfgs[name]
        feats = [f for f, pred in FEATURES if pred(c) and f not in ("2+ writers",)]
        lines.append(f"| `{name}` | {len(c['_invariants'])} | "
                     f"{c.get('Writers', 'TwoWriters (life lease)')} | {c['_paths']} | "
                     f"{', '.join(feats) if feats else '—'} |")

    out = "\n".join(lines) + "\n"
    target = HERE / "COVERAGE.md"
    if check:
        if not target.exists() or target.read_text() != out:
            print("COVERAGE.md is stale: re-run lean/formal/coverage.py", file=sys.stderr)
            return 1
        print("COVERAGE.md is current")
        return 0
    target.write_text(out)
    holes = sum(1 for _, _, cells in rows for x in cells if x == 0)
    print(f"wrote {target.name}: {len(invariants)} invariants x {len(FEATURES)} features, "
          f"{holes} unchecked pairs")
    return 0


if __name__ == "__main__":
    sys.exit(main())
