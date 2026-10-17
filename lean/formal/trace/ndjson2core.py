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
  claim verdict=claimed how=deposed -> depose        Depose(w): the takeover of a
                                                     held cell, its rotation the
                                                     seq it bumps (orphaned-own:
                                                     refused)
  fence (the first in a barrier)   -> fence          Fence(w): before its CAS the
                                                     barrier is abandoned, after it
                                                     the collector is skipped
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
  fold verb=Publish                -> touch          Touch(w): the request folded
                                                     into the standing record
  barrier_start while a record     -> (the consume)  its carry clock: the
    stands                                           barrier's `source`
  ack status=ok|partial            -> honor          Honor(w): status, dropped,
                                    + retire         seq, clock, nonces; then
                                                     RetireByAck(w)
  rescope stage=begin              -> rescope_begin  RescopeBegin(w): the target
                                                     (the paths its prefixes
                                                     cover, `Scope::covers`) and
                                                     the recorded drop set
  rescope stage=uncited            -> rescope_first  RescopeFirst(w): what it kept
  rescope stage=unlink (each)      -> (into the next RescopeSecond: a crash
                                                     part-way through the loop
                                                     leaves some unlinked, which
                                                     the model's one-step second
                                                     half does in the replay)
  rescope stage=applied            -> rescope_second RescopeSecond(w): every unlink
                                                     since the uncite and what the
                                                     widen took (a barrier's
                                                     step-0 replay emits these)
  sync (a writer's, whole-tree)    -> sync           Sync(w): the paths whose
                                                     citation it moved and the
                                                     ones it left the agent
                                                     (scoped, or one that left
                                                     something owed: refused)
  pull skipped=false (a reader's)  -> pull           RPullRead(w), at the seq read
  sync (the reader's, after it)    -> rpull_sync     RPullSync(w), as sync
  pull skipped=true                -> pull_skip      no step: the model's
                                                     ReaderSkips must hold there
  ack status=refused-read-only     -> honor_reader   HonorReader(w); then
                                    + retire         RetireByAck(w)
  start (a new incarnation's       -> restart        Restart(w): the barrier a
    `release_stale_own`)                             crash left in flight is
                                                     dropped; a takeover rotation
                                                     at the start is refused
  (no event)                       -> a Collect that takes nothing (TraceNext)
  claim verdict=waiting, gc per path, queue, sweep of manifest generations,
    release, handed_off, observed (folded into verify), conf_touch (the
    test's marker; the code's step is `fold`),
    renew (a held lease renewed: the model's holder is unchanged; a renew
    that FAILS logs `fence`, which is refused)
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
    readers = set()
    for e in evs:
        if e["ev"] == "conf_start":
            writer_of[e["holder"]] = e["writer"]
            if e.get("reader"):
                readers.add(e["writer"])
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
    # The publish request (rows 15-17): a record stands from a fold to the
    # ack that retires it. The model numbers a writer's touches 1, 2, ...;
    # a nonce maps to the touch that first carried it, and an ack's nonces
    # are checked only when every fold carried a nonce of its own.
    touches = {}   # w -> folds so far
    standing = {}  # w -> a record stands
    nonce_n = {}   # (w, nonce) -> the model's touch number
    exact = {}     # w -> every fold of the standing record added one nonce
    restarts = 0
    deposals = {}  # w -> takeovers it made
    rescopes = 0
    syncs = 0
    pulling = {}  # reader -> a pull read the pointer, its sync not yet traced
    # Scopes are resolved to paths at the end, when every path is known.
    scoped = []  # (record, field, prefixes or None)
    unlinks = {}  # w -> the narrow's unlinks since its uncite, across a crash

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
            rec = {"ev": "checkout", "w": e["writer"]}
            scoped.append((rec, "scope", None))  # the conformance writers check out unscoped
            steps.append((rec, i))
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
            clk = "none"
            if standing.get(w):
                # A barrier that begins while a record stands carries it
                # (`barrier_inner`), under the clock it stamps.
                clk = e["source"]
                if clk not in ("sentinel", "sentinel-deferred", "drain"):
                    raise Unsupported(f"a barrier carrying a declaration under clock {clk}")
            open_consume[w] = {"adopted": {}, "removed": set(), "clk": clk}
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
                how = e.get("how")
                if how == "deposed":
                    deposals[w] = deposals.get(w, 0) + 1
                    steps.append(({"ev": "depose", "w": w}, i))
                elif how == "orphaned-own":
                    raise Unsupported("an orphaned-own claim (an acquire whose container died)")
                else:
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
                           "foreign": m["foreign"], "gone": m["gone"], "surfaced": set(), "sdel": set()}, i))
            barrier[w]["install"] = steps[-1][0]
        elif ev == "surface":
            # A record written after the CAS, naming the version this install
            # published over. It belongs to that install.
            # A record of a DELETE this install went over names the deleted
            # version by etag alone, which a re-upload of the same bytes
            # shares: it is passed by path, and the check reads the version
            # the tombstone names (DeletedAt). Otherwise by key, then etag.
            if e.get("deleted"):
                barrier[w]["install"]["sdel"].add(e["path"])
            else:
                h = by_key.get(e.get("key")) or ver(e["path"], e["etag"], "surface")
                barrier[w]["install"]["surfaced"].add((e["path"], h))
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
        elif ev == "fence":
            # A deposed holder's first read of the cell; the ones after it in
            # the same barrier (a renew's, then the commit's) are the same fence.
            if not barrier.get(w, {}).get("fenced"):
                barrier.setdefault(w, {})["fenced"] = True
                steps.append(({"ev": "fence", "w": w}, i))
        elif ev == "rescope":
            st = e["stage"]
            if st == "begin":
                rescopes += 1
                paths.update(e["drop"])
                rec = {"ev": "rescope_begin", "w": w, "drop": set(e["drop"])}
                scoped.append((rec, "tgt", e["target"]))
            elif st == "uncited":
                rec = {"ev": "rescope_first", "w": w, "keep": set(e["kept"])}
            elif st == "unlink":
                unlinks.setdefault(w, set()).add(e["path"])
                continue
            elif st == "applied":
                paths.update(e["taken"])
                done = unlinks.pop(w, set())
                if not set(e["unlinked"]) <= done:
                    raise Unsupported(f"an applied narrow names unlinks no unlink event showed: {e['unlinked']}")
                rec = {"ev": "rescope_second", "w": w, "unlinked": done, "taken": set(e["taken"])}
            else:
                raise Unsupported(f"rescope stage {st}")
            steps.append((rec, i))
        elif ev == "pull":
            if e["skipped"]:
                steps.append(({"ev": "pull_skip", "w": w}, i))
            else:
                pulling[w] = True
                rec = {"ev": "pull", "w": w}
                if e.get("seq") is not None:
                    rec["seq"] = seq_of(e)
                steps.append((rec, i))
        elif ev == "sync":
            if e["scoped"]:
                raise Unsupported("a scoped sync (the sync request): only the whole-tree sync is modelled")
            if e["left"]:
                raise Unsupported("a sync that left something owed: the traces fail no fetch")
            paths.update(e["taken"])
            paths.update(e["skipped"])
            rec = {"ev": "sync", "w": w, "taken": set(e["taken"]), "skipped": set(e["skipped"])}
            if w in readers:
                if not pulling.pop(w, False):
                    raise Unsupported("a reader's sync with no pull before it")
                rec["ev"] = "rpull_sync"
            else:
                syncs += 1
            steps.append((rec, i))
        elif ev == "start":
            if e["rotated"]:
                raise Unsupported("a start that rotated the manifest (an acquire whose container died)")
            restarts += 1
            # What the crashed incarnation had in flight is gone with it.
            open_consume.pop(w, None)
            barrier.pop(w, None)
            steps.append(({"ev": "restart", "w": w}, i))
        elif ev == "fold":
            if e["verb"] != "Publish":
                raise Unsupported(f"a {e['verb']} request: only publish is modelled")
            touches[w] = touches.get(w, 0) + 1
            n = touches[w]
            if e["fresh"]:
                exact[w] = True
            if e["fresh"] != (not standing.get(w)):
                raise Unsupported("a fold the trace's records disagree on (fresh vs standing)")
            standing[w] = True
            new = [x for x in e["nonces"] if (w, x) not in nonce_n]
            if len(new) == 1:
                nonce_n[(w, new[0])] = n
            else:
                exact[w] = False
            steps.append(({"ev": "touch", "w": w}, i))
        elif ev == "ack" and e["status"] == "refused-read-only":
            if e["verb"] != "Publish" or not standing.get(w):
                raise Unsupported(f"a {e['verb']} refusal with no record standing")
            steps.append(({"ev": "honor_reader", "w": w}, i))
            steps.append(({"ev": "retire", "w": w}, i))
            standing[w] = False
        elif ev == "ack":
            if e["verb"] != "Publish" or e["status"] not in ("ok", "partial"):
                raise Unsupported(f"a {e['verb']} ack {e['status']}")
            if not standing.get(w):
                raise Unsupported("an ack with no record standing")
            rec = {"ev": "honor", "w": w, "st": e["status"], "dropped": set(e["dropped"]),
                   "src": e["boundary"]}
            if e.get("seq") is not None:
                rec["seq"] = seq_of(e)
            if exact.get(w):
                rec["nonces"] = {nonce_n[(w, x)] for x in e["nonces"]}
            steps.append((rec, i))
            steps.append(({"ev": "retire", "w": w}, i))
            standing[w] = False
        elif ev in ("queue", "release", "handed_off", "conf_touch", "drill_hold", "renew"):
            pass
        else:
            raise Unsupported(f"event {ev}")

    def covers(prefixes, p):
        if prefixes is None:
            return True
        es = [x.strip("/").replace("\\", "/") for x in prefixes]
        return any(p == x or p.startswith(x + "/") for x in es if x)

    scopes = {frozenset(paths)}
    for rec, field, prefixes in scoped:
        rec[field] = {p for p in paths if covers(prefixes, p)}
        scopes.add(frozenset(rec[field]))
    # An adds_nothing install keeps the seq; the model says so, not the trace.
    for r, _ in steps:
        if r["ev"] == "install" and r["seq"] is None:
            del r["seq"]
    consts = {
        "Writers": set(names), "Free": paths - set(seed), "Paths": paths,
        "MaxMint": max_mint, "MaxUI": hitl, "MaxRemovals": removals,
        "MaxBarriers": barriers + 1, "MaxRestarts": restarts, "MaxSyncs": syncs, "MaxCopies": copies,
        "MaxAges": max(reaps, 1),
        # THE CODE'S SHAPE: every rule on.
        **{k: True for k in (
            "CommitSurfacesForeign", "CommitVerifiesUploads", "SweepUnderLease",
            "CollectorSparesCited", "CommitRecordsDeleteOverride", "DeleteWinsPreserved",
            "ContentConverges", "RecheckSkipped", "CommitAdvanceGuarded", "RetireAge", "GatewayIgnoresLease", "GatewayJudgesRead",
            "GatewaySweepGrace", "RenameAtomic",
            "ConsumeHonorsScope", "RescopeUnciteFirst", "RescopeKeepsDirty", "WidenKeepsLocal",
            "UnlinkChecksBytes", "ConsumeKeepsLeft", "SyncKeepsLeft", "ReaderRechecksOwed",
            "InstallFenced", "DeletesUnderLease")},
        # The traces check out unscoped and never fail a fetch.
        "Scopes": scopes, "MaxRescopes": rescopes, "MaxFetchFails": 0, "Readers": readers,
        # Rows 15-17: the publish requests the trace folded; no deposal yet.
        "MaxTouches": max(touches.values(), default=0), "MaxNonces": 32, "MaxDeposals": max(deposals.values(), default=0),
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
        "Inv_NoRegress", "Inv_ShortcutSound", "Inv_ReaderFetches", "Inv_ReaderSound", "AK3", "AK4")]
    cfg += ["PROPERTY Prop_NoSilentRevert", "PROPERTY Prop_DeleteSettles", "PROPERTY AK1", "PROPERTY NR1",
            "PROPERTY Prop_ScopeRespected", "PROPERTY Prop_AgentWorkKept", "PROPERTY Prop_NarrowNeverDeletes",
            "PROPERTY DP1", "PROPERTY DP2"]
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
