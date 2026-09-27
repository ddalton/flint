#!/usr/bin/env python3
"""Turn a syncer event trace into a behaviour LeanP1.tla must produce.

Input: the NDJSON a `tests_conformance.rs` scenario writes, from `conf_start`.
(Earlier targets: LeanCore.tla, then LeanP2.tla; each version is archived
with its last run in ../results/2026-09-25-tracecore-on-{leancore,leanp2}/.)

Output, in <outdir>:
  TraceCoreData.tla  EXTENDS TraceCore; `CodeTrace == << ... >>`, one
                     model step per element
  TraceCore.cfg      LeanP2's constants as the code runs (every rule on),
                     with budgets that fit this trace exactly, and the
                     model's safety claims checked while it replays
  MAP.txt            which trace line each step came from; the version map

A VERSION is the model's handle <<minted-at path, gen>>: the seed is gen 1
at every seeded path, and each agent or UI write mints the next gen. A
re-upload of bytes already PUT (a withheld upload's) lands at a fresh key
in the code and at a COPY handle <<path, MaxMint + n>> in the model, and
that handle names those bytes from then on. The code names a version by
(path, etag) or by its key; a rename's destination names the SOURCE's
handle, as the model's citation move does.

THE MAPPING (one place):

  conf_start                       -> checkout       Checkout(w); the seed's keys
  conf_agent_write                 -> agent_write    Edit(w,p), gen = next mint
  conf_agent_delete                -> agent_delete   Delete(w,p)
  conf_hitl_write                  -> ui_put         GPut(p), gen = next mint
                                   +  ui_commit      GCas(p): it LANDED, at the seq logged
  conf_hitl_delete                 -> ui_delete      GDelete(p), at the seq logged
  conf_hitl_rename                 -> ui_rename      GRename(p,q), at the seq logged;
                                                     q's version = p's handle
  barrier_start + consume* +
    tombstone* (closed by scan,
    or by barrier_end)             -> consume        Consume(w): the tree's
                                                     adoptions (converged ones
                                                     included) and removals —
                                                     none on the cheap path
  scan                             -> scan           Scan(w), upload/delete counts
  barrier_end with no scan         -> fastpath       Skip(w)
  upload (put)                     -> upload         Upload(w,p): the version, and
                                                     the copy it landed at if any
  claim verdict=claimed            -> claim          Claim(w)
  merge, claimed                   -> verify         Verify(w), what the re-read withheld
  merge adds_nothing, claimed      -> install        Install(w) installing nothing
  merge + cas ok (+ surface*)      -> install        Install(w): the deletes the
                                                     merge OUTRANKED and applied
                                                     OVER theirs (M3), the seq, the
                                                     foreign/gone counts, the
                                                     versions recorded (R7 and
                                                     the delete override)
  merge, not claimed               -> pullonly       PullOnly(w), foreign/gone counts
  gc with a `retired` count        -> collect        Collect(w), retired count
  sweep what=orphans               -> sweep (each)   Sweep(w,h), the handle named;
                                                     a handle an UPLOAD put names
                                                     is that upload's, and its
                                                     Upload step moves up to here
  sweep what=retired               -> reap (each)    Reap(w,h): a handle a retire log
                                                     named, past the retire age
                                                     (M1); the age itself elapses
                                                     silently (TraceNext's Age)
  window_clear                     -> finish         Finish(w)
  (no event)                       -> a Collect that takes nothing (TraceNext)
  claim verdict=waiting, gc per path, queue, sweep of manifest generations,
    release, handed_off, observed (folded into verify), ack, conf_touch
                                   -> (nothing)

What the model does not have is refused, not skipped: a lost CAS (one
holder), a same-bytes write (the model mints every write), a swept key no
event named, and any cell event (repair, removal): the cell carries none.
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
    seed = seq0 = seed_keys = None
    for e in evs:
        if e["ev"] == "conf_start":
            writer_of[e["holder"]] = e["writer"]
            seed, seq0, seed_keys = e["entries"], e["seq"], e.get("keys", {})
    if seed is None:
        raise Unsupported("no conf_start")
    names = sorted(set(writer_of.values()))
    # Every write mints one generation; copies are numbered above them.
    max_mint = 1 + sum(1 for e in evs if e["ev"] in ("conf_agent_write", "conf_hitl_write"))

    ver_of = {(p, et): (p, 1) for p, et in seed.items()}  # (path, etag) -> handle
    by_key = {k: (p, 1) for p, k in seed_keys.items()}   # an object key -> handle
    upped = {(p, 1) for p in seed}                        # every handle PUT
    copies = 0
    nxt = 2
    paths = set(seed)
    steps = []  # (record, source line index)
    hitl = barriers = removals = reaps = 0
    open_consume = {}  # w -> dict
    # An upload's key -> its event index: a peer's sweep can name it before
    # the uploading barrier traces it.
    up_key = {e["key"]: j for j, e in enumerate(evs) if e["ev"] == "upload" and e.get("key")}
    early = set()  # upload events already stepped at a sweep
    barrier = {}  # w -> dict(claimed, observed, merge, scanned)

    def ver(p, et, what):
        if (p, et) not in ver_of:
            raise Unsupported(f"{what}: {p} at etag {et} has no version (a write the trace did not show)")
        return ver_of[(p, et)]

    def seq_of(e):
        return e["seq"] - seq0 + 1

    def upload(j):
        # The version uploaded, and — bytes PUT once before — the copy it
        # lands at, which names those bytes from here on.
        nonlocal copies
        u = evs[j]
        p = u["path"]
        h = ver(p, u["etag"], "upload")
        rec = {"ev": "upload", "w": writer_of[u["holder"]], "p": p, "h": h}
        at = h
        if h in upped:
            copies += 1
            at = (p, max_mint + copies)
            ver_of[(p, u["etag"])] = at
            rec["at"] = at
        upped.add(at)
        if u.get("key"):
            if u["key"] in by_key:
                raise Unsupported(f"an upload to a key already written: {u['key']} (a handle is immutable)")
            by_key[u["key"]] = at
        steps.append((rec, j))

    def close_consume(w, i):
        c = open_consume.pop(w, None)
        if c is not None:
            # The consume's NET effect on the tree.
            adopted = {(p, h) for p, h in c.pop("adopted").items() if p not in c["removed"]}
            steps.append(({"ev": "consume", "w": w, "adopted": adopted, **c}, i))

    for i, e in enumerate(evs):
        ev = e["ev"]
        w = writer_of.get(e.get("holder"))
        # A gateway event inside a barrier (a test window's) lands after that
        # barrier's consume: the consume's events are all traced by then.
        if ev in ("conf_hitl_write", "conf_hitl_delete", "conf_hitl_rename"):
            for ow in list(open_consume):
                close_consume(ow, i)
        if ev == "conf_start":
            steps.append(({"ev": "checkout", "w": e["writer"]}, i))
        elif ev in ("conf_agent_write", "conf_hitl_write"):
            p, et = e["path"], e["etag"]
            paths.add(p)
            if (p, et) in ver_of:
                raise Unsupported(f"{ev} of bytes {p} already had: the model mints every write")
            ver_of[(p, et)] = (p, nxt)
            if ev == "conf_agent_write":
                steps.append(({"ev": "agent_write", "w": e["writer"], "p": p, "g": nxt}, i))
            else:
                by_key[e["key"]] = (p, nxt)
                upped.add((p, nxt))
                steps.append(({"ev": "ui_put", "p": p, "g": nxt}, i))
                commit = {"ev": "ui_commit", "p": p}
                if "seq" in e:
                    commit["seq"] = seq_of(e)
                steps.append((commit, i))
                hitl += 1
            nxt += 1
        elif ev == "conf_agent_delete":
            paths.add(e["path"])
            steps.append(({"ev": "agent_delete", "w": e["writer"], "p": e["path"]}, i))
        elif ev == "conf_hitl_delete":
            removals += 1
            rec = {"ev": "ui_delete", "p": e["path"]}
            if "seq" in e:
                rec["seq"] = seq_of(e)
            steps.append((rec, i))
        elif ev == "conf_hitl_rename":
            src, dst = e["from"], e["to"]
            paths.add(dst)
            removals += 1
            # A citation move: the destination names the source's handle.
            ver_of[(dst, e["etag"])] = ver(src, e["etag"], "rename source")
            rec = {"ev": "ui_rename", "p": src, "q": dst}
            if "seq" in e:
                rec["seq"] = seq_of(e)
            steps.append((rec, i))
        elif ev == "barrier_start":
            barriers += 1
            open_consume[w] = {"adopted": {}, "removed": set()}
            barrier[w] = {"claimed": False, "observed": {}, "merge": None, "scanned": False}
        elif ev == "consume":
            c = open_consume[w]
            p, et, act = e["path"], e["etag"], e["action"]
            if e.get("from") != "manifest":
                raise Unsupported(f"a consume from the {e.get('from')}: the tree is owed only what the document cites")
            if act in ("adopted", "converged"):
                c["adopted"][p] = ver(p, et, f"consume {act}")
            else:
                # missing / refused: a cited handle is live in the model.
                raise Unsupported(f"consume action {act}")
        elif ev == "tombstone":
            c = open_consume[w]
            act = e["action"]
            if act == "removed":
                c["removed"].add(e["path"])
            elif act in ("absent", "kept-dirty"):
                pass
            else:
                raise Unsupported(f"tombstone action {act}")
        elif ev == "scan":
            close_consume(w, i)
            barrier[w]["scanned"] = True
            steps.append(({"ev": "scan", "w": w, "nuploads": e["uploads"], "ndeletes": e["deletes"]}, i))
        elif ev == "upload":
            if i in early:
                continue
            if e["outcome"] != "put":
                raise Unsupported(f"upload outcome {e['outcome']}")
            upload(i)
        elif ev == "claim":
            v = e["verdict"]
            if v == "claimed":
                barrier[w]["claimed"] = True
                steps.append(({"ev": "claim", "w": w}, i))
            elif v != "waiting":
                raise Unsupported(f"claim verdict {v}")
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
                               "over": set(e.get("over_theirs", [])), "seq": None, **counts}, i))
        elif ev == "cas":
            m = barrier[w]["merge"]
            if e["result"] != "ok":
                raise Unsupported("a lost CAS: the model's commit section has one holder")
            steps.append(({"ev": "install", "w": w, "outranked": set(m["outranked"]),
                           "over": set(m.get("over_theirs", [])), "seq": seq_of(e),
                           "foreign": m["foreign"], "gone": m["gone"], "surfaced": set()}, i))
            barrier[w]["install"] = steps[-1][0]
        elif ev == "surface":
            # A record written after the CAS, naming the version this install
            # published over. It belongs to that install.
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
            if e.get("what") == "retired":
                reaps += 1
                for k in e["keys"]:
                    if k not in by_key:
                        raise Unsupported(f"a reaped handle no event named: {k}")
                    steps.append(({"ev": "reap", "w": w, "h": by_key[k]}, i))
            elif e.get("what") == "orphans":
                for k in e["keys"]:
                    if k not in by_key and k in up_key and up_key[k] > i:
                        early.add(up_key[k])
                        upload(up_key[k])
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
        "MaxMint": max_mint, "MaxUI": hitl, "MaxRemovals": removals,
        "MaxBarriers": barriers + 1, "MaxRestarts": 0, "MaxSyncs": 0, "MaxCopies": copies,
        "MaxAges": max(reaps, 1),
        # THE CODE'S SHAPE: every rule on.
        **{k: True for k in (
            "CommitSurfacesForeign", "CommitVerifiesUploads", "SweepUnderLease",
            "CollectorSparesCited", "CommitRecordsDeleteOverride", "DeleteWinsPreserved",
            "ContentConverges", "RecheckSkipped", "CommitAdvanceGuarded", "RetireAge", "GatewayIgnoresLease", "GatewayJudgesRead",
            "GatewaySweepGrace", "RenameAtomic")},
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
        "TypeOK", "Inv_CitationsLive", "Inv_OneName", "Inv_AckedNamed", "Inv_OneHolder",
        "Inv_NoRegress", "Inv_ShortcutSound", "Inv_ReaderFetches")]
    cfg += ["PROPERTY Prop_NoSilentRevert", "PROPERTY Prop_DeleteSettles"]
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
