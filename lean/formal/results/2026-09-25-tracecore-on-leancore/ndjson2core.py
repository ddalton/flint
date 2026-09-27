#!/usr/bin/env python3
"""Turn a syncer event trace into a behaviour LeanCore.tla must produce.

The LeanCore twin of ndjson2tla.py (which targets LeanSubtree.tla). Input:
the NDJSON a `tests_conformance.rs` scenario writes, from `conf_start`.

Output, in <outdir>:
  TraceCoreData.tla  EXTENDS TraceCore; `CodeTrace == << ... >>`, one
                     LeanCore step per element
  TraceCore.cfg      LeanCore's constants as shipped (every design rule on),
                     with budgets that fit this trace exactly, and the
                     core's safety claims checked while it replays
  MAP.txt            which trace line each step came from; the version map

A VERSION is LeanCore's handle <<minted-at path, gen>>: the seed is gen 1
at every seeded path, and each agent or UI write mints the next gen. The
code names a version by (path, etag) or, for a UI write, by its handle key;
a rename's destination names the SOURCE's handle, as the core's citation
move does.

THE MAPPING (one place):

  conf_start                       -> checkout       Checkout(w)
  conf_agent_write                 -> agent_write    Edit(w,p), gen = next mint
  conf_agent_delete                -> agent_delete   Delete(w,p)
  conf_hitl_write                  -> hitl_write     UIWrite(p), gen = next mint
  conf_hitl_delete                 -> hitl_delete    UIDelete(p)
  conf_hitl_rename                 -> hitl_rename    UIRename(p,q), q's version = p's handle
  barrier_start + consume* +
    tombstone* + removal* (closed
    by scan, or by barrier_end)    -> consume        Consume(w), the tree's
                                                     adoptions, removals,
                                                     preserved copies, and
                                                     the removals it refused
  scan                             -> scan           Scan(w), upload/delete counts
  barrier_end with no scan         -> fastpath       Skip(w)
  upload (put)                     -> upload         Upload(w,p), the version checked
  claim verdict=claimed            -> claim          Claim(w)
  merge, claimed                   -> verify         Verify(w), what the re-read withheld
  merge adds_nothing, claimed      -> install        Install(w) installing nothing
  repair* + merge + cas ok
    (+ surface*)                   -> install        Install(w): the deletes the
                                                     merge OUTRANKED, the seq, the
                                                     foreign/gone counts, the
                                                     versions R7 surfaced, and
                                                     the adoptions kept PENDING
  merge, not claimed               -> pullonly       PullOnly(w), foreign/gone counts
  gc with a `retired` count        -> collect        Collect(w), retired count
  sweep what=orphans               -> sweep (each)   Sweep(w,h), the handle named;
                                                     a handle an UPLOAD put names
                                                     is that upload's, and its
                                                     Upload step moves up to here
                                                     (the put LANDED before the
                                                     sweep; the code traces the
                                                     batch's puts after the last)
  window_clear                     -> finish         Finish(w)
  (no event)                       -> a Collect that takes nothing (TraceNext)
  claim verdict=waiting, gc per path, queue, sweep of manifest generations,
    release, handed_off, observed (folded into verify), ack, conf_touch
                                   -> (nothing)

What the core does not model is refused, not skipped: a lost CAS (the core's
commit section has one holder), a same-bytes write (the core mints every
write), a swept key no event named.
"""
import json
import sys
from pathlib import Path


def tla(v):
    if isinstance(v, bool):
        return "TRUE" if v else "FALSE"
    if isinstance(v, int):
        return str(v)
    if isinstance(v, str):
        return json.dumps(v)
    if isinstance(v, tuple):
        return "<<" + ", ".join(tla(x) for x in v) + ">>"
    if isinstance(v, (set, frozenset)):
        return "{" + ", ".join(sorted(tla(x) for x in v)) + "}"
    if isinstance(v, dict):
        return "[" + ", ".join(f"{k} |-> {tla(x)}" for k, x in v.items()) + "]"
    raise TypeError(v)


class Unsupported(Exception):
    pass


def convert(lines):
    evs = [json.loads(l) for l in lines if l.strip()]
    writer_of = {}
    seed = seq0 = None
    for e in evs:
        if e["ev"] == "conf_start":
            writer_of[e["holder"]] = e["writer"]
            seed, seq0 = e["entries"], e["seq"]
    if seed is None:
        raise Unsupported("no conf_start")
    names = sorted(set(writer_of.values()))

    ver_of = {(p, et): (p, 1) for p, et in seed.items()}  # (path, etag) -> handle
    by_key = {}  # a UI write's handle key -> handle
    nxt = 2
    paths = set(seed)
    steps = []  # (record, source line index)
    hitl = barriers = removals = 0
    max_seq = 1
    open_consume = {}  # w -> dict
    # An upload's handle key -> its event index: a peer's sweep can name it
    # before the uploading barrier traces it.
    up_key = {e["key"]: j for j, e in enumerate(evs) if e["ev"] == "upload" and e.get("key")}
    early = set()  # upload events already stepped at a sweep
    barrier = {}  # w -> dict(claimed, observed, merge, scanned)

    def ver(p, et, what):
        if (p, et) not in ver_of:
            raise Unsupported(f"{what}: {p} at etag {et} has no version (a write the trace did not show)")
        return ver_of[(p, et)]

    def close_consume(w, i):
        c = open_consume.pop(w, None)
        if c is not None:
            # The check is on the consume's NET effect on the tree: a path
            # adopted and then removed by a declared removal in the same
            # consume (an answered record, applied once the queue brought
            # the named version) is a removal.
            adopted = {(p, h) for p, h in c.pop("adopted").items() if p not in c["removed"]}
            steps.append(({"ev": "consume", "w": w, "adopted": adopted, **c}, i))

    for i, e in enumerate(evs):
        ev = e["ev"]
        w = writer_of.get(e.get("holder"))
        if ev == "conf_start":
            steps.append(({"ev": "checkout", "w": e["writer"]}, i))
        elif ev in ("conf_agent_write", "conf_hitl_write"):
            p, et = e["path"], e["etag"]
            paths.add(p)
            if (p, et) in ver_of:
                raise Unsupported(f"{ev} of bytes {p} already had: the core mints every write")
            ver_of[(p, et)] = (p, nxt)
            if e.get("key"):
                by_key[e["key"]] = (p, nxt)
            if ev == "conf_agent_write":
                steps.append(({"ev": "agent_write", "w": e["writer"], "p": p, "g": nxt}, i))
            else:
                steps.append(({"ev": "hitl_write", "p": p, "g": nxt}, i))
                hitl += 1
            nxt += 1
        elif ev == "conf_agent_delete":
            paths.add(e["path"])
            steps.append(({"ev": "agent_delete", "w": e["writer"], "p": e["path"]}, i))
        elif ev == "conf_hitl_delete":
            removals += 1
            steps.append(({"ev": "hitl_delete", "p": e["path"]}, i))
        elif ev == "conf_hitl_rename":
            src, dst = e["from"], e["to"]
            paths.add(dst)
            removals += 1
            # A citation move: the destination names the source's handle.
            ver_of[(dst, e["etag"])] = ver(src, e["etag"], "rename source")
            steps.append(({"ev": "hitl_rename", "p": src, "q": dst}, i))
        elif ev == "barrier_start":
            barriers += 1
            open_consume[w] = {"adopted": {}, "removed": set(), "dirty": set(), "kept": set(),
                               "refused": set()}
            barrier[w] = {"claimed": False, "observed": {}, "merge": None, "scanned": False, "pending": set()}
        elif ev == "consume":
            c = open_consume[w]
            p, et, act = e["path"], e["etag"], e["action"]
            if act == "adopted":
                c["adopted"][p] = ver(p, et, "consume adopted")
            elif act == "dirty-preserved":
                c["dirty"].add((p, ver(p, et, "consume dirty")))
            elif act in ("already", "superseded", "missing"):
                pass
            else:
                raise Unsupported(f"consume action {act}")
        elif ev == "tombstone":
            c = open_consume[w]
            act = e["action"]
            if act == "removed":
                c["removed"].add(e["path"])
            elif act == "kept-dirty":
                c["kept"].add(e["path"])
            elif act in ("absent", "superseded"):
                pass
            else:
                raise Unsupported(f"tombstone action {act}")
        elif ev == "removal":
            c = open_consume[w]
            act = e["action"]
            if act == "applied":
                if e["unlinked"]:
                    c["removed"].add(e["path"])
            elif act == "refused":
                c["refused"].add(e["path"])
            elif act not in ("declined", "deferred"):
                raise Unsupported(f"removal action {act}")
        elif ev == "scan":
            close_consume(w, i)
            barrier[w]["scanned"] = True
            steps.append(({"ev": "scan", "w": w, "nuploads": e["uploads"], "ndeletes": e["deletes"]}, i))
        elif ev == "upload":
            if i in early:
                continue
            if e["outcome"] != "put":
                raise Unsupported(f"upload outcome {e['outcome']}")
            p = e["path"]
            steps.append(({"ev": "upload", "w": w, "p": p, "h": ver(p, e["etag"], "upload")}, i))
        elif ev == "claim":
            v = e["verdict"]
            if v == "claimed":
                barrier[w]["claimed"] = True
                steps.append(({"ev": "claim", "w": w}, i))
            elif v != "waiting":
                raise Unsupported(f"claim verdict {v}")
        elif ev == "repair":
            # The commit's repair rule, per destination adoption. "pending"
            # is L-119 (declined for a move, kept in the cell); the rest are
            # what the model's Install decides without naming.
            if e["action"] == "pending":
                barrier[w]["pending"].add(e["path"])
            elif e["action"] not in ("voided", "entombed", "superseded", "no-handle"):
                raise Unsupported(f"repair action {e['action']}")
        elif ev == "observed":
            barrier[w]["observed"][e["path"]] = e["still"]
        elif ev == "merge":
            b = barrier[w]
            b["merge"] = e
            counts = {"foreign": e["foreign"], "gone": e["gone"]}
            if not b["claimed"]:
                steps.append(({"ev": "pullonly", "w": w, **counts}, i))
                continue
            steps.append(({"ev": "verify", "w": w,
                           "withheld": {p for p, s in b["observed"].items() if not s}}, i))
            if e["adds_nothing"]:
                steps.append(({"ev": "install", "w": w, "outranked": set(e["outranked"]),
                               "seq": None, **counts, "pending": set(b["pending"])}, i))
        elif ev == "cas":
            m = barrier[w]["merge"]
            if e["result"] != "ok":
                raise Unsupported("a lost CAS: the core's commit section has one holder")
            s = e["seq"] - seq0 + 1
            max_seq = max(max_seq, s)
            steps.append(({"ev": "install", "w": w, "outranked": set(m["outranked"]), "seq": s,
                           "foreign": m["foreign"], "gone": m["gone"], "surfaced": set(),
                           "pending": set(barrier[w]["pending"])}, i))
            barrier[w]["install"] = steps[-1][0]
        elif ev == "surface":
            # R7's record, written after the CAS: theirs' version at a path
            # this install published over. It belongs to that install.
            barrier[w]["install"]["surfaced"].add((e["path"], ver(e["path"], e["etag"], "surface")))
        elif ev == "gc":
            if "retired" in e:
                steps.append(({"ev": "collect", "w": w, "retired": e["retired"]}, i))
        elif ev == "window_clear":
            steps.append(({"ev": "finish", "w": w}, i))
        elif ev == "barrier_end":
            close_consume(w, i)
            if not barrier.get(w, {}).get("scanned", True):
                steps.append(({"ev": "fastpath", "w": w}, i))
            barrier.pop(w, None)
        elif ev == "sweep":
            if e.get("what") == "orphans":
                for k in e["keys"]:
                    if k not in by_key and k in up_key and up_key[k] > i:
                        j = up_key[k]
                        u = evs[j]
                        h = ver(u["path"], u["etag"], "swept upload")
                        steps.append(({"ev": "upload", "w": writer_of[u["holder"]], "p": u["path"], "h": h}, j))
                        early.add(j)
                        by_key[k] = h
                    if k not in by_key:
                        raise Unsupported(f"a swept handle no event named: {k}")
                    steps.append(({"ev": "sweep", "w": w, "h": by_key[k]}, i))
            elif e.get("removed", 0) > 0 and e.get("what") not in ("generations", "chunks"):
                raise Unsupported(f"a sweep of {e.get('what')}")
        elif ev in ("queue", "release", "handed_off", "ack", "conf_touch", "drill_hold"):
            pass
        else:
            raise Unsupported(f"event {ev}")

    # An adds_nothing install keeps the seq; the model says so, not the trace.
    for r, _ in steps:
        if r["ev"] == "install" and r["seq"] is None:
            del r["seq"]
    consts = {
        "Writers": set(names), "Free": paths - set(seed), "Paths": paths,
        "MaxMint": max(nxt - 1, 1), "MaxSeq": max_seq + 1, "MaxUI": hitl, "MaxRemovals": removals,
        "MaxBarriers": barriers + 1,
        # THE SHIPPED SHAPE: every design rule on, as in LeanCoreHolds.
        **{k: True for k in (
            "CommitSurfacesForeign", "RepairRespectsMoves", "RenameMovesEntry",
            "AnsweredRecordsApply", "RepairYieldsToLaterUI", "CollectorSparesCited",
            "SweepSparesNamed", "CommitVerifiesUploads", "SweepUnderLease",
            "PendingAdoptionStays", "ForeignPerPath", "OutrankedRemovalLeavesBaseline",
            "ParkedKeepsMergeBase", "CommitRecordsDeleteOverride")},
    }
    return steps, consts, ver_of


def write_out(steps, consts, gen, outdir: Path, name: str, overrides=None):
    outdir.mkdir(parents=True, exist_ok=True)
    body = ",\n  ".join(tla(r) for r, _ in steps)
    (outdir / "TraceCoreData.tla").write_text(
        f"---- MODULE TraceCoreData ----\n\\* Generated by ndjson2core.py from {name}; do not edit.\n"
        f"EXTENDS TraceCore\nCodeTrace == <<\n  {body}\n>>\n====\n")
    c = dict(consts)
    c.update(overrides or {})
    cfg = ["INIT TraceInit", "NEXT TraceNext", "CHECK_DEADLOCK FALSE", "CONSTANTS", "  Nil = Nil"]
    cfg += [f"  {k} = {tla(v)}" for k, v in c.items()]
    cfg += ["  Trace <- CodeTrace"]
    # The core's claims, on the run the code actually performed.
    cfg += [f"INVARIANT {i}" for i in (
        "TypeOK", "Inv_CitationsLive", "Inv_OneName", "Inv_AckedNamed", "Inv_OneHolder")]
    cfg += ["PROPERTY Prop_NoSilentRevert"]
    cfg += ["INVARIANT TraceProgress", "INVARIANT TraceIncomplete"]
    (outdir / "TraceCore.cfg").write_text("\n".join(cfg) + "\n")
    m = [f"{n + 1}\t{r['ev']}\tline {i + 1}" for n, (r, i) in enumerate(steps)]
    m += ["", "versions:"] + [f"  {p} {et} -> {h}" for (p, et), h in sorted(gen.items(), key=lambda x: x[1][1])]
    (outdir / "MAP.txt").write_text("\n".join(m) + "\n")


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--set=")]
    overrides = {}
    for a in sys.argv[1:]:
        if a.startswith("--set="):
            k, v = a[len("--set="):].split("=", 1)
            overrides[k] = {"TRUE": True, "FALSE": False}.get(v, int(v) if v.isdigit() else v)
    src, outdir = Path(args[0]), Path(args[1])
    lines = src.read_text().splitlines()
    try:
        steps, consts, gen = convert(lines)
    except Unsupported as e:
        print(f"UNSUPPORTED {src.name}: {e}")
        return 2
    write_out(steps, consts, gen, outdir, src.name, overrides)
    print(f"{src.name}: {len(lines)} trace lines -> {len(steps)} model steps"
          + (f" (overrides {overrides})" if overrides else ""))
    return 0


if __name__ == "__main__":
    sys.exit(main())
