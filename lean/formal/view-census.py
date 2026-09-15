#!/usr/bin/env python3
"""The soundness census for LeanSubtree's `StrictView` (run first by check.sh).

A TLC VIEW merges states that differ only outside the view. That is sound
exactly when no dropped field can influence anything the view keeps: no
action guard, no update of a kept field, no checked invariant. This script
checks that syntactically, failing CLOSED on anything it cannot place:

  * the `gh` fields are those of Init's `gh = [...]` record; the kept ones
    are `StrictGh`'s; the rest are DROPPED;
  * the dropped `sc` fields are the ones `StrictSc` resets;
  * every read of a dropped field must sit in a `Probe*` definition, or in
    the right-hand side of an update (`[gh EXCEPT !.f = ...]`,
    `[sc EXCEPT ![s].f = ...]`) whose target is itself dropped;
  * `gh` as a whole may appear only as `gh'`, `[gh EXCEPT`, `gh = [` in
    Init, or inside an UNCHANGED / vars / view tuple;
  * `StrictView` must be `vars` with sc -> StrictSc and gh -> StrictGh, so a
    variable added to the module and not to the view fails here.

`--selftest` runs the positive controls: six edits that each make the
view unsound must each fail the census, and the unedited file must pass.
"""
import re
import sys


def strip_comments(src: str) -> str:
    """Blank out (* nested *) and \\* line comments, keeping offsets."""
    out = list(src)
    i, depth, n = 0, 0, len(src)
    while i < n:
        if src.startswith("(*", i):
            depth += 1
            out[i] = out[i + 1] = " "
            i += 2
            continue
        if depth and src.startswith("*)", i):
            depth -= 1
            out[i] = out[i + 1] = " "
            i += 2
            continue
        if depth:
            if src[i] != "\n":
                out[i] = " "
            i += 1
            continue
        if src.startswith("\\*", i):
            j = src.find("\n", i)
            j = n if j < 0 else j
            for k in range(i, j):
                out[k] = " "
            i = j
            continue
        i += 1
    return "".join(out)


def matching(src: str, start: int, open_: str, close: str) -> int:
    depth, i = 0, start
    while i < len(src):
        if src.startswith(open_, i):
            depth += 1
            i += len(open_)
            continue
        if src.startswith(close, i):
            depth -= 1
            if depth == 0:
                return i
            i += len(close)
            continue
        i += 1
    raise ValueError(f"unbalanced {open_} at {start}")


def definition(src: str, name: str) -> str:
    m = re.search(rf"^{name}\s*==", src, re.M)
    if not m:
        raise ValueError(f"no definition {name}")
    nxt = re.search(r"^\S", src[m.end():], re.M)
    return src[m.start(): m.end() + (nxt.start() if nxt else len(src))]


def enclosing_def(src: str, pos: int) -> str:
    best = ""
    for m in re.finditer(r"^([A-Za-z_]\w*)(\([^)]*\))?\s*==", src[:pos], re.M):
        best = m.group(1)
    return best


def enclosing_brackets(src: str, pos: int):
    """Stack of (opener, start) enclosing pos: '[', '(', '{', '<<'."""
    stack = []
    i = 0
    while i < pos:
        two = src[i:i + 2]
        if two == "<<":
            stack.append(("<<", i))
            i += 2
            continue
        if two == ">>" and stack and stack[-1][0] == "<<":
            stack.pop()
            i += 2
            continue
        c = src[i]
        if c in "[({":
            stack.append((c, i))
        elif c in "])}" and stack:
            stack.pop()
        i += 1
    return stack


def update_target(src: str, pos: int, record: str):
    """If pos is inside `[<record> EXCEPT ...]`, the field the nearest
    preceding top-level `!.f =` / `![x].f =` in that bracket assigns."""
    for opener, start in reversed(enclosing_brackets(src, pos)):
        if opener != "[":
            continue
        if not re.match(rf"\[\s*{re.escape(record)}\s+EXCEPT\b", src[start:]):
            continue
        body = src[start:pos]
        # top-level assignments only: skip those nested deeper
        targets = []
        depth = 0
        k = 0
        while k < len(body):
            ch = body[k]
            if body.startswith("<<", k):
                depth += 1
                k += 2
                continue
            if body.startswith(">>", k):
                depth -= 1
                k += 2
                continue
            if ch in "[({" and k > 0:
                depth += 1
            elif ch in "])}":
                depth -= 1
            if depth == 0 and body.startswith("!", k):
                m = re.match(r"!(?:\[[^\]]*\])?\.(\w+)(?:\[[^\]]*\])?\s*=", body[k:])
                if m:
                    targets.append(m.group(1))
            k += 1
        return targets[-1] if targets else None
    return None


def census(src_raw: str):
    src = strip_comments(src_raw)
    errors = []

    init = definition(src, "Init")
    gm = re.search(r"\bgh\s*=\s*\[", init)
    gh_rec = init[gm.end() - 1: matching(init, gm.end() - 1, "[", "]") + 1]
    universe = set(re.findall(r"(\w+)\s*\|->", gh_rec))

    strict_gh = definition(src, "StrictGh")
    kept = set()
    for name, field in re.findall(r"(\w+)\s*\|->\s*gh\.(\w+)", strict_gh):
        if name != field:
            errors.append(f"StrictGh maps {name} to gh.{field}")
        kept.add(field)
    for f in kept - universe:
        errors.append(f"StrictGh keeps gh.{f}, which Init does not define")
    dropped_gh = universe - kept

    strict_sc = definition(src, "StrictSc")
    dropped_sc = set(re.findall(r"!\.(\w+)\s*=", strict_sc))

    def tuple_of(name):
        d = definition(src, name)
        i = d.index("<<")
        return [t.strip() for t in d[i + 2: matching(d, i, "<<", ">>")].split(",")]

    want = ["StrictSc" if v == "sc" else "StrictGh" if v == "gh" else v for v in tuple_of("vars")]
    if tuple_of("StrictView") != want:
        errors.append(f"StrictView is not vars with sc/gh projected:\n  vars ~ {want}\n  view = {tuple_of('StrictView')}")

    allowed_defs = {"StrictGh", "StrictSc", "StrictView"}

    def line_of(pos):
        return src.count("\n", 0, pos) + 1

    def check_read(pos, field, kind):
        d = enclosing_def(src, pos)
        if d.startswith("Probe") or d in allowed_defs:
            return
        tgt_gh = update_target(src, pos, "gh")
        if tgt_gh is not None and tgt_gh in dropped_gh:
            return
        tgt_sc = update_target(src, pos, "sc")
        if tgt_sc is not None and tgt_sc in dropped_sc and tgt_gh is None:
            return
        errors.append(
            f"line {line_of(pos)} ({d}): {kind}.{field} is DROPPED from StrictView but read here "
            f"(update target gh:{tgt_gh} sc:{tgt_sc})"
        )

    for m in re.finditer(r"\bgh\.(\w+)", src):
        if m.group(1) in dropped_gh:
            check_read(m.start(), m.group(1), "gh")
        elif m.group(1) not in universe:
            errors.append(f"line {line_of(m.start())}: gh.{m.group(1)} is not a field Init defines")
    # `sc[...]` with a BALANCED index (`sc[Writers[j]].f` nests): a regex
    # that stopped at the first `]` both missed such reads and flagged them
    # as whole-record reads.
    def sc_accesses():
        for m in re.finditer(r"\bsc\[", src):
            close = matching(src, m.end() - 1, "[", "]")
            fm = re.match(r"\.(\w+)", src[close + 1:])
            yield m, close, (fm.group(1) if fm else None)
    for m, close, field in sc_accesses():
        if field in dropped_sc:
            check_read(m.start(), field, "sc")

    vm = re.search(r"^VARIABLES\b", src, re.M)
    vend = re.search(r"^[A-Za-z_]\w*\s*==", src[vm.end():], re.M) if vm else None
    var_span = (vm.start(), vm.end() + (vend.start() if vend else 0)) if vm else (0, 0)

    def bare_ok(m, var):
        d = enclosing_def(src, m.start())
        if d in allowed_defs:
            return True
        if var_span[0] < m.start() < var_span[1]:
            return True  # the VARIABLES declaration itself
        if re.search(r"\[\s*$", src[max(0, m.start() - 12): m.start()]) and re.match(r"\s+EXCEPT", src[m.end():]):
            return True  # [v EXCEPT ...]: a copy, fields flow only into themselves
        if d == "Init" and re.match(r"\s*=\s*\[", src[m.end():]):
            return True
        tup = [st for o, st in enclosing_brackets(src, m.start()) if o == "<<"]
        if tup and re.search(r"(UNCHANGED|vars\s*==)\s*$", src[max(0, tup[-1] - 20): tup[-1]]):
            return True
        if re.search(r"UNCHANGED\s+$", src[max(0, m.start() - 12): m.start()]):
            return True
        return False

    for var in ("gh", "sc"):
        for m in re.finditer(rf"\b{var}\b", src):
            nxt = src[m.end(): m.end() + 1]
            if nxt == "'" or (var == "gh" and nxt == "."):
                continue
            if var == "sc" and nxt == "[":
                j = matching(src, m.end(), "[", "]")
                if src[j + 1: j + 2] in (".", "["):
                    continue  # sc[x].field: a field read, checked above
                if enclosing_def(src, m.start()) in allowed_defs:
                    continue
                errors.append(f"line {line_of(m.start())} ({enclosing_def(src, m.start())}): whole-record read of sc[...] reads the dropped fields too")
                continue
            if not bare_ok(m, var):
                errors.append(f"line {line_of(m.start())} ({enclosing_def(src, m.start())}): bare `{var}` read outside a copy or UNCHANGED tuple")

    return errors, sorted(kept), sorted(dropped_gh), sorted(dropped_sc)


def selftest(src: str) -> int:
    errs, *_ = census(src)
    if errs:
        print("selftest: the UNEDITED module fails the census:\n  " + "\n  ".join(errs))
        return 1
    edits = {
        "a guard reads a dropped counter": (
            "  /\\ gh.touches < MaxTouches\n",
            "  /\\ gh.touches < MaxTouches /\\ gh.acks < 5\n",
        ),
        "a KEPT field's update reads a dropped counter": (
            "!.narrows = @ + 1,",
            "!.narrows = @ + 1 + gh.done - gh.done,",
        ),
        "a dropped sc field feeds a kept one": (
            "![s].honored = FALSE,\n            ![s].pendReRun = IF SentinelEnabled",
            "![s].honored = sc[s].pendReRun,\n            ![s].pendReRun = IF SentinelEnabled",
        ),
        "a dropped sc field read through a nested index": (
            "  /\\ gh.touches < MaxTouches\n",
            "  /\\ gh.touches < MaxTouches /\\ sc[Writers[1]].pendReRun\n",
        ),
        "a whole sc record compared in a guard": (
            "  /\\ gh.touches < MaxTouches\n",
            "  /\\ gh.touches < MaxTouches /\\ sc[s] # sc[s]\n",
        ),
        "a variable added to vars but not to the view": (
            "          conflicts, gh>>\n",
            "          conflicts, gh, window>>\n",
        ),
    }
    rc = 0
    for label, (old, new) in edits.items():
        if src.count(old) < 1:
            print(f"selftest: cannot apply control '{label}' — its anchor text moved; update the selftest")
            rc = 1
            continue
        errs, *_ = census(src.replace(old, new, 1))
        if errs:
            print(f"selftest ok: '{label}' fails the census ({errs[0][:100]})")
        else:
            print(f"selftest FAILED: '{label}' PASSES the census — the census has no teeth there")
            rc = 1
    return rc


def main() -> int:
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    path = args[0] if args else "LeanSubtree.tla"
    src = open(path).read()
    if "--selftest" in sys.argv:
        return selftest(src)
    errs, kept, dropped, dropped_sc = census(src)
    if errs:
        print("view census FAILED — StrictView is not sound for this module:")
        for e in errs:
            print("  " + e)
        return 1
    print(f"view census ok: {len(kept)} gh fields kept, {len(dropped)} dropped "
          f"({', '.join(dropped)}); sc fields dropped: {', '.join(dropped_sc)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
