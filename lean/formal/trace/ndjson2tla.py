#!/usr/bin/env python3
"""Turn a syncer event trace into a behaviour LeanSubtree.tla must produce.

Input: the NDJSON a `tests_conformance.rs` scenario writes (the protocol
event trace of every writer, plus the harness's `conf_*` events for what
the trace cannot see), starting at `conf_start`.

Output, in <outdir>:
  TraceData.tla   `Trace == << [ev |-> ..., ...], ... >>` — one model step
                  per element
  TraceLean.cfg   the model's constants for this trace (the code's shape:
                  barrier lease, writer queue, empty install, two-scan
                  deletes) with budgets that fit it exactly
  MAP.txt         which trace line each step came from, and the etag ->
                  generation map

THE MAPPING (one place; README "Trace validation" quotes it):

  conf_start (per writer)          -> start            StartLease(w)
  conf_agent_write                 -> agent_write      AgentWrite(w,p), gen = next mint
                                                       (AgentWriteSame when the
                                                       (path, etag) already has one)
  conf_agent_delete                -> agent_delete     AgentDelete(w,p)
  conf_hitl_write                  -> hitl_write       HitlWrite(p), gen = next mint
  conf_touch                       -> touch, take      Touch(w); TakeSentinel(w)
                                                       (the harness polls at once)
  barrier_start + consume* +
    tombstone* (closed by scan,
    or by barrier_end)             -> consume          Consume(w), with the tree's
                                                       adoptions and removals checked
  scan                             -> scan             Scan(w), upload/delete counts
  barrier_end with no scan         -> fastpath         FastPath(w)
  upload                           -> upload           Upload(w,p), outcome checked
  claim verdict=claimed            -> claim            Claim(w) or SkipDeadHandoff(w)
  claim verdict=waiting            -> wait             Enqueue(w), or a stutter
  merge with no claim this barrier -> pullonly         PullOnly(w), queue counts
  observed*, merge, cas ok         -> install          CASInstall(w), seq + withheld
  merge adds_nothing (claimed)     -> install          CASInstall(w) installing nothing
  cas lost                         -> cas_lost         CASMiss(w)
  gc                               -> gc               GCDelete(w,p), result checked
  window_clear                     -> finish           Finish(w)
  ack                              -> ack, retire      AckOk/AckPartial(w); RetirePending(w)
  queue, sweep, release, handed_off, barrier_end, drill_hold  -> (nothing)

A GC the code skips without an event (a delete the merge outranked) is the
one step the model takes silently (TraceLean.tla).
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
    seed = None
    seq0 = None
    for e in evs:
        if e["ev"] == "conf_start":
            writer_of[e["holder"]] = e["writer"]
            seed, seq0 = e["entries"], e["seq"]
    names = sorted(set(writer_of.values()))
    if names != list("ABCDEF"[:len(names)]) or len(names) < 2:
        raise Unsupported(f"writers must be A, B, ... in order, got {writer_of}")
    nwriters = len(names)

    projected = any(e["ev"] == "conf_projected" for e in evs)
    gen = {(p, et): 1 for p, et in seed.items()}
    nxt = 2
    paths = set(seed)
    steps = []  # (record, source line index)
    hitl = touches = scans = fast = same = 0
    max_seq = 1

    open_consume = {}  # w -> dict
    barrier = {}  # w -> dict(claimed, observed, merge)

    def gen_of(p, et, what):
        if (p, et) not in gen:
            raise Unsupported(f"{what}: {p} at etag {et} has no generation (a write the trace did not show)")
        return gen[(p, et)]

    def close_consume(w, i):
        c = open_consume.pop(w, None)
        if c is None:
            return
        steps.append(({"ev": "consume", "w": w, "adopted": c["adopted"], "removed": c["removed"],
                       "dirty": c["dirty"], "kept": c["kept"]}, i))

    for i, e in enumerate(evs):
        ev = e["ev"]
        w = writer_of.get(e.get("holder"))
        if ev == "conf_projected":
            continue
        if ev == "conf_abandon":
            steps.append(({"ev": "abandon", "w": e["writer"]}, i))
            barrier.pop(writer_of.get(e.get("holder"), e["writer"]), None)
            continue
        if ev == "conf_start":
            steps.append(({"ev": "start", "w": e["writer"]}, i))
        elif ev == "conf_agent_write":
            p, et = e["path"], e["etag"]
            paths.add(p)
            if (p, et) in gen:
                same += 1
                steps.append(({"ev": "agent_write", "w": e["writer"], "p": p, "g": gen[(p, et)], "same": True}, i))
            else:
                gen[(p, et)] = nxt
                steps.append(({"ev": "agent_write", "w": e["writer"], "p": p, "g": nxt, "same": False}, i))
                nxt += 1
        elif ev == "conf_agent_delete":
            paths.add(e["path"])
            steps.append(({"ev": "agent_delete", "w": e["writer"], "p": e["path"]}, i))
        elif ev == "conf_hitl_write":
            p, et = e["path"], e["etag"]
            paths.add(p)
            if (p, et) in gen:
                raise Unsupported(f"a UI write of bytes {p} already had: the model mints every UI write")
            gen[(p, et)] = nxt
            steps.append(({"ev": "hitl_write", "p": p, "g": nxt}, i))
            nxt += 1
            hitl += 1
        elif ev == "conf_touch":
            touches += 1
            steps.append(({"ev": "touch", "w": e["writer"]}, i))
            steps.append(({"ev": "take", "w": e["writer"]}, i))
        elif ev == "barrier_start":
            open_consume[w] = {"adopted": set(), "removed": set(), "dirty": set(), "kept": set()}
            barrier[w] = {"claimed": False, "observed": {}, "merge": None, "scanned": False}
            # `barrier.rs` step 1 reads the cell ONCE, here, and the consume
            # below integrates THAT snapshot. Its own step, so a peer's
            # window clear can land in between — which is what the replay
            # of churn/p47.txt needed (W4 phase 2).
            steps.append(({"ev": "load", "w": w}, i))
        elif ev == "consume":
            c = open_consume[w]
            p, et, act = e["path"], e["etag"], e["action"]
            if act == "adopted":
                c["adopted"].add((p, gen_of(p, et, "consume adopted")))
            elif act == "dirty-preserved":
                c["dirty"].add((p, gen_of(p, et, "consume dirty")))
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
        elif ev == "scan":
            close_consume(w, i)
            barrier[w]["scanned"] = True
            scans += 1
            rec = {"ev": "scan", "w": w}
            if "uploads" in e:      # absent in a PROJECTED trace: a whole-leg count
                rec.update({"uploads": e["uploads"], "deletes": e["deletes"]})
            steps.append((rec, i))
        elif ev == "upload":
            out = e["outcome"]
            p = e["path"]
            if out in ("put", "adopted"):
                steps.append(({"ev": "upload", "w": w, "p": p, "outcome": out, "g": gen_of(p, e["etag"], "upload")}, i))
            elif out == "parked":
                steps.append(({"ev": "upload", "w": w, "p": p, "outcome": out, "g": 0}, i))
            else:
                raise Unsupported(f"upload outcome {out}")
        elif ev == "claim":
            v = e["verdict"]
            if v == "claimed":
                barrier[w]["claimed"] = True
                steps.append(({"ev": "claim", "w": w}, i))
            elif v == "waiting":
                steps.append(({"ev": "wait", "w": w}, i))
            else:
                raise Unsupported(f"claim verdict {v}")
        elif ev == "observed":
            barrier[w]["observed"][e["path"]] = e["still"]
        elif ev == "merge":
            b = barrier[w]
            b["merge"] = e
            counts = {"foreign": e["foreign"], "gone": e["gone"]} if "foreign" in e else {}
            seqf = {} if projected else {"seq": 0, "nothing": True}
            if not b["claimed"]:
                steps.append(({"ev": "pullonly", "w": w, **counts}, i))
            elif e["adds_nothing"]:
                steps.append(({"ev": "install", "w": w, **seqf, **counts,
                               "withheld": {p for p, s in b["observed"].items() if not s}}, i))
        elif ev == "cas":
            b = barrier[w]
            m = b["merge"]
            if e["result"] == "ok":
                s = e["seq"] - seq0 + 1
                max_seq = max(max_seq, s)
                counts = {"foreign": m["foreign"], "gone": m["gone"]} if "foreign" in m else {}
                seqf = {} if projected else {"seq": s, "nothing": False}
                steps.append(({"ev": "install", "w": w, **seqf, **counts,
                               "withheld": {p for p, st in b["observed"].items() if not st}}, i))
            else:
                steps.append(({"ev": "cas_lost", "w": w}, i))
            b["merge"] = None
        elif ev == "gc":
            steps.append(({"ev": "gc", "w": w, "p": e["path"], "result": e["result"]}, i))
        elif ev == "window_clear":
            steps.append(({"ev": "finish", "w": w}, i))
        elif ev == "barrier_end":
            if w in open_consume:
                close_consume(w, i)
            if not barrier.get(w, {}).get("scanned", True):
                fast += 1
                steps.append(({"ev": "fastpath", "w": w}, i))
            barrier.pop(w, None)
        elif ev == "ack":
            steps.append(({"ev": "ack", "w": w, "status": e["status"]}, i))
            steps.append(({"ev": "retire", "w": w}, i))
        elif ev in ("queue", "sweep", "release", "handed_off", "drill_hold"):
            pass
        else:
            raise Unsupported(f"event {ev}")

    free = paths - set(seed)
    consts = {
        "MaxGen": max(nxt - 1, 1), "MaxSeq": max_seq + 2, "MaxHitl": hitl,
        "MaxBarriers": scans + fast + 1, "MaxCrashes": 0, "MaxRestarts": 0, "MaxSyncs": 0,
        "AllowStall": False, "InboxEnabled": True, "MergeCapable": True, "ConflictSurfacing": True,
        "WindowCheck": True, "Rotation": True, "EpochCheck": True, "GuardedGC": True,
        "DeletesAfterCAS": True, "RematerializeOnRestart": False, "SyncEnabled": False,
        "SyncScanFirst": True, "SyncScope": False, "ScopedInstBase": True, "GatedCitation": False,
        "AtomicCitation": True, "GCKeepsCurrent": True, "CiteDropsInflightHitl": True,
        "BackstopEnabled": False, "MineIsNotForeign": True, "MaxTouches": touches,
        "SentinelEnabled": touches > 0, "FoldPending": True, "AckFromInstall": True,
        "RefuseOnFence": True, "FastPathGuards": True, "AckHonest": True,
        "LaneCancelsStaged": False, "GatedRepair": False, "StampBoundarySource": True,
        "TwoScanDelete": True, "MaxNarrows": 0, "NarrowAtomic": True, "NarrowUnlinkFirst": False,
        "MaxRemovals": 0, "DeclaredSkipsWalk": True, "EarlyInboxDrop": False,
        "RenameWaitsForDestination": True, "BarrierLease": True, "Ticket": True,
        "DeadHandoffSkip": True, "InfiniteBarriers": False, "ConditionalGC": True,
        "VerifyAdoptedCitations": True, "HitlOverwritesTrackedOnly": True,
        "SyncKeepsHiddenBase": True, "MaxSameBytes": same, "VerifyUploadedCitations": True,
        "WriterQueue": True, "EmptyInstall": True, "TombstoneHeadsKey": True,
        "CommitLoadsCurrent": True, "Upload412Preserves": True, "DeclaredConfirmsAbsence": True,
        "Writers": "<- " + ["", "", "TwoWriters", "ThreeWriters", "FourWriters",
                            "FiveWriters", "SixWriters"][nwriters],
        "OrphanTrack": False, "OrphanEntryCited": True, "LeakSupersedesNothing": True, "ManifestTombstones": True, "SupersedeRestoresBase": True, "ProjectedTrace": projected,
        # The handoff rule the code implements (found by replaying the storm).
        "AbandonOnStoreError": True,
        "BaselineKeepsUncollected": True,
        # The epoch discipline, as it ships: every claim mints a new epoch
        # and stamps it on the claimant (`lease.rs`'s acquire).
        "ClaimMintsEpoch": True, "ClaimStampsEpoch": True,
        # The barrier reads the cell once, at its first step, and consumes
        # from that snapshot — which is what churn/p47.txt needed.
        "InboxSnapshot": True,
        # The replayed runs are all on S3, which enforces the conditional
        # DELETE, so the collector collects. A trace from a store that
        # fails `probe-conditional` would need this TRUE and the leg's
        # gc events would say "leaked" rather than "deleted".
        "CollectorOff": False,
        "QueueForeignChanges": True,
        # Review 2026-09-18: the code after C2 and H1 (a fence before every
        # delete is the module's shape; the tombstone carries what the
        # peer's delete retired).  A trace's "superseded" tombstone is a
        # DIFFERENT etag at the key on S3, so the replays are unchanged.
        "GCFencePerDelete": True, "TombstoneNamesRetired": True,
    }
    return steps, consts, paths, free, gen


def write_out(steps, consts, paths, free, gen, evs_count, outdir: Path, name: str, overrides=None):
    outdir.mkdir(parents=True, exist_ok=True)
    body = ",\n  ".join(tla(r) for r, _ in steps)
    (outdir / "TraceData.tla").write_text(
        f"---- MODULE TraceData ----\n\\* Generated by ndjson2tla.py from {name}; do not edit.\n"
        f"Trace == <<\n  {body}\n>>\n====\n")
    c = dict(consts)
    c.update(overrides or {})
    # The retired-etag rule is a sub-arm of the key rule under the writer
    # queue (the module's ASSUME): a control that turns either off turns
    # it off too, or the replay errors instead of rejecting.
    if not (c.get("TombstoneHeadsKey") and c.get("WriterQueue")):
        c["TombstoneNamesRetired"] = False
    cfg = ["INIT TraceInit", "NEXT TraceNext", "CHECK_DEADLOCK FALSE", "CONSTANTS",
           f"  Paths = {tla(set(paths))}", f"  FreePaths = {tla(set(free))}"]
    cfg += [f"  {k} {v}" if isinstance(v, str) and v.startswith("<-") else f"  {k} = {tla(v)}"
            for k, v in c.items()]
    # THE SAFETY INVARIANTS, CHECKED WHILE THE TRACE IS REPLAYED. Following
    # the trace only shows the model CAN do what the code did; these ask
    # whether the run the code actually performed ever entered a state the
    # model calls unsafe — a dangling citation, an acked UI write nothing
    # tracks, a resurrection, two writers in one commit section. That is the
    # check a data-loss defect would fail, on real bytes rather than in a
    # world TLC invented.
    cfg += [f"INVARIANT {i}" for i in (
        "TypeOK", "Inv_HITLDurable", "Inv_NoDangling", "Inv_NoStragglerInstall",
        "Inv_NoDeposedPut", "Inv_NoResurrection", "Inv_HITLTracked",
        "Inv_CommitExclusive", "Inv_CellHeldByHolder", "Inv_NoStaleOverride",
        # Convergence, which only bites where the replay comes to rest.
        "Inv_QuiescentConverged")]
    cfg += ["INVARIANT TraceProgress", "INVARIANT TraceIncomplete"]
    (outdir / "TraceLean.cfg").write_text("\n".join(cfg) + "\n")
    m = [f"{n + 1}\t{r['ev']}\tline {i + 1}" for n, (r, i) in enumerate(steps)]
    m += ["", "generations:"] + [f"  {p} {et} -> {g}" for (p, et), g in sorted(gen.items(), key=lambda x: x[1])]
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
        steps, consts, paths, free, gen = convert(lines)
    except Unsupported as e:
        print(f"UNSUPPORTED {src.name}: {e}")
        return 2
    write_out(steps, consts, paths, free, gen, len(lines), outdir, src.name, overrides)
    print(f"{src.name}: {len(lines)} trace lines -> {len(steps)} model steps"
          + (f" (overrides {overrides})" if overrides else ""))
    return 0


if __name__ == "__main__":
    sys.exit(main())
