#!/usr/bin/env python3
"""Generate formal/ForgeSyncProofM0.tla: the TLAPS TypeOK proof of ForgeSync.tla
(docs/plans/forge-tlaps-inductive-proof-plan.md, M0).

Pass 1 is COARSE on purpose: IndTypeOK == TypeOK, and every step lemma is one
`BY DEF` naming the step, TypeOK, and every definition the step reaches
(computed here, transitively, from the module text).  tlapm then says which
obligations need a hand-written decomposition; those are refined by name in
REFINED below and the rest stay generated.

Usage: gen-m0.py <ForgeSync.tla> <out.tla>
"""
import re
import sys

src_path, out_path = sys.argv[1], sys.argv[2]
text = open(src_path).read()

# ---- strip comments and strings so identifier scans see only code ----------
def strip(t):
    out, i, depth = [], 0, 0
    while i < len(t):
        if t.startswith("(*", i):
            depth += 1; i += 2; continue
        if depth and t.startswith("*)", i):
            depth -= 1; i += 2; continue
        if depth:
            i += 1; continue
        if t.startswith("\\*", i):
            j = t.find("\n", i); i = len(t) if j < 0 else j; continue
        if t[i] == '"':
            j = t.find('"', i + 1); out.append('""'); i = j + 1; continue
        out.append(t[i]); i += 1
    return "".join(out)

code = strip(text)
lines = code.split("\n")

# ---- top-level definitions: name -> (params, body text) ---------------------
DEF_RE = re.compile(r"^([A-Za-z][A-Za-z0-9_]*)(?:\(([^)]*)\))?\s*==")
starts = []
for n, ln in enumerate(lines):
    m = DEF_RE.match(ln)
    if m:
        starts.append((n, m.group(1), m.group(2)))
defs = {}
for k, (n, name, params) in enumerate(starts):
    end = starts[k + 1][0] if k + 1 < len(starts) else len(lines)
    body = "\n".join(lines[n:end])
    body = body[body.index("==") + 2:]
    if "====" in body:
        body = body[:body.index("====")]
    defs[name] = (params, body)

IDENT = re.compile(r"(?<![A-Za-z0-9_.])([A-Za-z][A-Za-z0-9_]*)")
def refs(name):
    """Definitions a definition's body mentions (direct)."""
    _, body = defs[name]
    found = set()
    for m in IDENT.finditer(body):
        tok = m.group(1)
        # a record label `x |->` or a field `x:` in a record set is not a reference
        tail = body[m.end():m.end() + 4]
        if tok in defs and tok != name and not tail.lstrip().startswith("|->"):
            found.add(tok)
    return found

NOT_DEFS = {"Init", "Next", "Spec", "Fairness", "Sym", "SymOverlap", "Toks", "Rank",
            "View", "TypeOK", "Inv", "DeadHolderWatchResolves", "SentPushResolves",
            "HeldByNamedComplete", "Durable"}
NOT_DEFS |= {d for d in defs if d.startswith("Inv_") or d.startswith("Probe")}

def closure(name):
    seen, todo = set(), [name]
    while todo:
        d = todo.pop()
        for r in refs(d):
            if r in NOT_DEFS or r in seen:
                continue
            seen.add(r); todo.append(r)
    return seen

# ---- the steps, read off Next --------------------------------------------------
_, next_body = defs["Next"]
steps = []
for m in re.finditer(r"([A-Z][A-Za-z0-9_]*)\(([a-z, ]+)\)", next_body):
    steps.append((m.group(1), [a.strip() for a in m.group(2).split(",")]))
assert len(steps) == 38, [s[0] for s in steps]
assert len({s[0] for s in steps}) == 38

TYPE_SETS = ["Stages", "States", "PushStates", "FoldStages", "PushIds", "PackIds"]
BOUND = {"s": "Syncers", "p": "Pushes"}

# TypeOK's conjuncts, one per top-level `/\` line (a continuation line joins
# the conjunct above it).  A step whose one-line proof timed out proves each
# of these primed as its own obligation (run 1: 12 steps, every one a timeout
# at 60-105 s of a 60 s budget, none a fast failure).
def typeok_conjuncts():
    _, body = defs["TypeOK"]
    assert '""' not in body, "TypeOK has a string literal the stripper blanked"
    out = []
    for ln in body.split("\n"):
        s = ln.strip()
        if not s:
            continue
        if s.startswith("/\\"):
            out.append(s[2:].strip())
        else:
            out[-1] += " " + s
    return out

CONJ = typeok_conjuncts()
# The steps run 1 could not close in one `BY DEF` (all timeouts).
SPLIT = {"Restore", "RenewTick", "BatchRenew", "BatchHash", "BatchComplete",
         "BatchCas", "FoldInit", "FoldRenew", "FoldCommit", "KeptSetRead",
         "KeptSetCommit", "CleanRelease"}

def prime(conj):
    """`v \\in S` -> `v' \\in S`: prime every state variable in a conjunct."""
    _, vbody = defs["vars"]
    names = set(IDENT.findall(vbody)) - {"RW"}
    names |= set(IDENT.findall(defs["RW"][1]))
    return IDENT.sub(lambda m: m.group(1) + "'" if m.group(1) in names else m.group(1), conj)

# Split steps that timed out NARROWLY (run 2: 62-72 s of a 60 s limit) get a
# larger SMT budget instead of a restructure: SMTT(20) is 240 s under
# --stretch 12 (micro/Budget.tla: tlapm reports time-limit 240).  Keyed by
# (lemma, step number); every entry is a step that FAILED at 60 s.
# EMPTY since run 3: the two budgeted steps it reached failed at 240 s too.
# The five were never slow -- they were FALSE from TypeOK alone (a batch in
# progress may carry push 0, a fold in progress id 0), and z3 answers an
# unprovable goal with a timeout.  F1/F2 below are the fix.
BUDGET = {}

# A split step z3 cannot close in one `BY DEF` although it is TRUE: the step
# is given the fact the action states, as its own sub-step.  Keyed by (lemma,
# step number); every entry FAILED at 60 s with the goal read and found true.
# KeptSetCommit <1>21 (fold' typed): run 5 76.6 s, run 6 (alone) 60 s.  Both
# branches of the IF (the commit, and Fall) set fold[s] to NoFold; z3 spends
# its budget on the other 25 variables the two branches assign.  The QED
# sees only the typing defs, not the action.
HINT = {
    ("KeptSetCommit", 21): ("fold' = [fold EXCEPT ![s] = NoFold]",
                            "KeptSetCommit, Fall",
                            "IndTypeOK, TypeOK, NoFold, FoldStages"),
}

def step_lemma(name, args):
    news = ", ".join(f"NEW {a} \\in {BOUND[a]}" for a in args)
    call = f"{name}({', '.join(args)})"
    head = (f"LEMMA {name}_TypeOK ==\n"
            f"  ASSUME {news}, IndTypeOK, {call}\n"
            f"  PROVE  IndTypeOK'\n")
    ds = sorted(closure(name) | set(TYPE_SETS))
    by = f"DEF IndTypeOK, TypeOK, F1, F2, {name}, " + ", ".join(ds)
    if name not in SPLIT:
        return head + "BY " + by + "\n"
    body = []
    for i, c in enumerate(CONJ, 1):
        smtt = f"SMTT({BUDGET[(name, i)]}) " if (name, i) in BUDGET else ""
        if (name, i) in HINT:
            fact, fact_defs, use_defs = HINT[(name, i)]
            body.append(f"<1>{i}. {prime(c)}\n"
                        f"  <2>1. {fact}\n    BY DEF {fact_defs}\n"
                        f"  <2>. QED BY <2>1 DEF {use_defs}")
            continue
        body.append(f"<1>{i}. {prime(c)}\n  BY {smtt}{by}")
    n = len(CONJ)
    body.append(f"<1>{n + 1}. F1'\n  BY {by}")
    body.append(f"<1>{n + 2}. F2'\n  BY {by}")
    body.append(f"<1>. QED BY " + ", ".join(f"<1>{i}" for i in range(1, n + 3))
                + " DEF IndTypeOK, TypeOK")
    return head + "\n".join(body) + "\n"

init_defs = sorted(closure("Init") | set(TYPE_SETS))

out = []
w = out.append
w("------------------------- MODULE ForgeSyncProofM0 ----------------------------")
w("""(* The TLAPS proof module for flint forge's push path, milestone M0 of
   docs/plans/forge-tlaps-inductive-proof-plan.md: `Spec => []TypeOK`.
   GENERATED by formal/gen-forgesyncproof-m0.py from formal/ForgeSync.tla;
   edit the generator, not this file.  It EXTENDS ForgeSync.tla itself --
   tlapm reads the module as written, EXTENDS TLC included (the parse
   smoke in results/2026-10-17-forgesync-tlaps-m0/) -- so there is no copy
   to tie and the model's md5 is the gate's.

   What a proof must be told and TLC reads off the cfg for free is stated
   as named ASSUMEs (tlapm does not use an unnamed one).  CodeRules is the
   code's combination, formal/ForgeSyncCode.cfg's booleans; M0 does not
   use it -- TypeOK holds under every rule -- but every later milestone
   does, and this module is their root.

   Each step lemma is one `BY DEF` over the step and every definition it
   reaches; a step the coarse form cannot close is refined by hand in the
   generator's REFINED table.                                             *)""")
w("EXTENDS ForgeSync, TLAPS, FiniteSetTheorems")
w("")
w("-" * 78)
w("(* What a proof must be told.                                               *)")
w("")
w("\\* The code's combination (formal/ForgeSyncCode.cfg); PacksOverlap is left free.")
w("""CodeRules ==
  /\\ IdxGate /\\ TickOnHash /\\ RotateOnTakeover /\\ AckAfterCas
  /\\ PacksBeforeCas /\\ SweepAtClaim /\\ GraceOutlivesUpload
  /\\ PollsNoFasterThanHeartbeat
  /\\ ReclaimAtRestore /\\ ReclaimBySet /\\ RestoreLosesQueue /\\ ReclaimUnlinks
  /\\ NameAcceptedSet /\\ NameNeeded /\\ ReclaimKeptSet /\\ RestoreDropsNamedRetention
  /\\ ~NamingUnretains
  /\\ ~FoldCasBeforeUpload /\\ ~FoldCasFromDisk /\\ ~FoldInputsAfterStart
  /\\ ~FoldCommitMidBatch /\\ ~SweepDuringFold /\\ ~FoldTicksBatchSensor
  /\\ ~FoldNoRenew /\\ ~FoldNoCoverageCheck /\\ ~FoldSupersedesArrivals
  /\\ ~FoldReachableCoverage /\\ ~ReclaimWhileServing /\\ ~ForgetPushPack
  /\\ ~ProveFromDisk /\\ ~ListingKeepsRetained /\\ ~NeededNamesQueued
  /\\ ~NeededTrustsDisk /\\ ~NeededSkipsRetained
  /\\ ~KeptSetAnyDrop /\\ ~KeptSetVsOriginal /\\ ~KeptSetWhileServing
  /\\ ~KeptSetNoRenew
ASSUME CodeShape == CodeRules
""")
w("\\* What the cfg's model values and finite constants give TLC for free.")
w("ASSUME NoSyncerDistinct == NoSyncer \\notin Syncers")
w("ASSUME PushesNonEmpty   == Pushes # {}")
w("ASSUME ZeroNotPack      == 0 \\notin Pushes /\\ 0 \\notin FoldIds")
w("ASSUME PushesFoldsApart == Pushes \\cap FoldIds = {}")
w("ASSUME MissesNat        == Misses \\in Nat")
w("ASSUME BudgetsNat       == /\\ MaxCrashes \\in Nat /\\ MaxRenews \\in Nat /\\ MaxClaims \\in Nat")
w("                           /\\ MaxFolds \\in Nat /\\ MaxRewinds \\in Nat /\\ MaxResends \\in Nat")
w("ASSUME SetsFinite       == IsFiniteSet(Syncers) /\\ IsFiniteSet(Pushes) /\\ IsFiniteSet(FoldIds)")
w("")
w("USE NoSyncerDistinct, PushesNonEmpty, ZeroNotPack, PushesFoldsApart, MissesNat, BudgetsNat, SetsFinite")
w("")
w("-" * 78)
w("(* M0: TypeOK.  TypeOK alone is NOT inductive (runs 1-3): BatchCas,       *)")
w("(* FoldInit and FoldCommit write a batch's push or a fold's id where a push *)")
w("(* or a pack must be, and TypeOK lets both be 0 mid-flight.  F1/F2 close it *)")
w("(* (TLC-checked first: results/<date>-forgesync-tlaps-m0/).                *)")
w("")
w("\\* F1: a batch in progress has a real push.")
w('F1 == \\A s \\in Syncers : batch[s].stage # "none" => batch[s].push \\in Pushes')
w("\\* F2: a fold in progress has a real fold id -- except the kept-set reclaim,")
w("\\* which holds the slot at \"keptRenewed\" with id 0 by design (KeptSetRead).")
w('F2 == \\A s \\in Syncers : fold[s].stage \\notin {"none", "keptRenewed"} => fold[s].id \\in FoldIds')
w("IndTypeOK == TypeOK /\\ F1 /\\ F2")
w("")
w("LEMMA Init_TypeOK == Init => IndTypeOK")
w("BY DEF Init, IndTypeOK, TypeOK, F1, F2, " + ", ".join(init_defs))
w("")
for name, args in steps:
    w(step_lemma(name, args))
w("LEMMA Next_TypeOK == IndTypeOK /\\ Next => IndTypeOK'")
w("<1>. SUFFICES ASSUME IndTypeOK, Next PROVE IndTypeOK' OBVIOUS")
labels = []
for i, (name, args) in enumerate(steps, 1):
    news = ", ".join(f"NEW {a} \\in {BOUND[a]}" for a in args)
    call = f"{name}({', '.join(args)})"
    w(f"<1>{i}. ASSUME {news}, {call} PROVE IndTypeOK' BY <1>{i}, {name}_TypeOK")
    labels.append(f"<1>{i}")
w("<1>. QED BY " + ", ".join(labels) + " DEF Next")
w("")
w("LEMMA Unchanged_TypeOK == IndTypeOK /\\ UNCHANGED vars => IndTypeOK'")
w("BY DEF IndTypeOK, TypeOK, F1, F2, vars, RW")
w("")
w("THEOREM TypeInvariant == Spec => []TypeOK")
w("<1>1. Init => IndTypeOK BY Init_TypeOK")
w("<1>2. IndTypeOK /\\ [Next]_vars => IndTypeOK' BY Next_TypeOK, Unchanged_TypeOK")
w("<1>3. IndTypeOK => TypeOK BY DEF IndTypeOK")
w("<1>. QED BY <1>1, <1>2, <1>3, PTL DEF Spec")
w("")
w("=" * 78)
open(out_path, "w").write("\n".join(out) + "\n")
print(f"{out_path}: {len(steps)} step lemmas, {len(out)} lines")
for name, args in steps:
    print(f"  {name:18s} {len(closure(name)):3d} defs")
