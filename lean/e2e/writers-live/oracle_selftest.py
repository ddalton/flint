#!/usr/bin/env python3
"""oracle_selftest.py — an oracle that cannot fail proves nothing.

Builds synthetic collected legs in a temp dir and runs oracle.py on each
exactly as the driver does (a subprocess; verdict JSON on stdout; exit code).

- `clean`: a leg with a sequential edit, a concurrent edit preserved with
  its record, a rename, a UI write, a `partial` ack dropping a path and a
  no-ack batch. Every oracle must PASS.
- one FAULT scenario per injected defect: EXACTLY the named oracle(s) fail.
- NON-FAULT scenarios that must pass through ONE accounting arm of O3 with
  the other arms shut (final content differs, no preserved copy, no later
  base) — each paired with a CONTROL that closes that one arm and must fail.
  A pass that survives its own arm being removed would be vacuous.

- the REAL syncer trace fixture (testdata/trace-two-writers.jsonl) through
  timeline.py and O5/O6: every line read, the gc of x.txt ordered before the
  other holder's tombstone of it, no deposal or deadline counted.

    oracle_selftest.py [--keep DIR] [--only NAME[,NAME…]]

Exit 0 iff every scenario's failing-oracle set is the expected one and every
fixture check holds.
"""
import argparse
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
from collections import defaultdict

HERE = os.path.dirname(os.path.abspath(__file__))
ORACLE = os.path.join(HERE, "oracle.py")
PREFIX = "ws/writers"
IDLE = ["--idle-from", "100000", "--idle-to", "200000", "--request-baseline", "5"]


def H(label):
    return hashlib.sha256(label.encode()).hexdigest()


def ckey(uuid, path):
    return f"{PREFIX}/.flint/lean/conflicts/{uuid}/{path}"


class Leg:
    """The evidence of one collected leg, written out by `write`."""

    def __init__(self):
        self.agents = ["a1", "a2"]
        self.meta = {"leg": "selftest", "prefix": PREFIX, "floor_secs": 5, "agents": list(self.agents),
                     "t_start_ms": 0, "t_quiesce_ms": 60000, "t_end_ms": 300000}
        self.journal = defaultdict(list)
        self.final = {}                      # path -> (sha256, etag)
        self.tree_extra = defaultdict(dict)  # agent -> {path: sha}
        self.tree_missing = defaultdict(set)
        self.tree_differ = defaultdict(dict)
        self.heads_override = {}             # path -> head entry (or DELETE)
        self.exit = {"code": 0, "stderr_tail": ""}
        self.preserved = []
        self.conflicts = defaultdict(list)
        self.traces = defaultdict(list)
        self.chrony = {}                     # node -> [(ts, off)]
        self.args = list(IDLE)

    # journal lines
    def op(self, agent, n, t, op, path, nonce, sha=None, base="absent", to=None, to_base=None):
        d = {"k": "op", "agent": agent, "n": n, "t_ms": t, "op": op, "path": path}
        if op == "mv":
            d.update({"to": to, "base": base, "to_base": to_base or "absent", "sha256": sha})
        elif op == "write":
            d.update({"sha256": sha, "base": base})
        else:
            d.update({"base": base})
        d["nonce"] = nonce
        self.journal[agent].append(d)

    def ack(self, agent, t, nonce, status, seq=None, covered=None, dropped=None, etag=None):
        d = {"k": "ack", "agent": agent, "t_ms": t, "nonce": nonce, "status": status}
        if agent == "ui":
            if etag:
                d["etag"] = etag
        elif status != "no-ack":
            d.update({"seq": seq, "dropped": dropped or [], "covered": covered or [nonce],
                      "report": {"uploaded": 1, "deleted": 0, "parked": 0, "consumed": 1, "no_change": False}})
        self.journal[agent].append(d)

    def ev(self, agent, ts, ev, **kw):
        # the common prefix as BUILT (README §2): no `flush` unless the event carries one
        d = {"ts_ms": ts, "mono_ms": ts, "holder": f"h-{agent}", "ev": ev}
        d.update(kw)
        self.traces[agent].append(d)

    def write(self, root):
        def dump(rel, obj):
            p = os.path.join(root, rel)
            os.makedirs(os.path.dirname(p), exist_ok=True)
            with open(p, "w") as f:
                json.dump(obj, f, indent=1)

        def lines(rel, objs, prose=()):
            p = os.path.join(root, rel)
            os.makedirs(os.path.dirname(p), exist_ok=True)
            with open(p, "w") as f:
                for s in prose:
                    f.write(s + "\n")
                for o in objs:
                    f.write(json.dumps(o, separators=(",", ":")) + "\n")

        def digest(rel, files):
            p = os.path.join(root, rel)
            os.makedirs(os.path.dirname(p), exist_ok=True)
            with open(p, "w") as f:
                for path in sorted(files):
                    f.write(f"{files[path]}  {path}\n")

        dump("meta.json", self.meta)
        final = {p: s for p, (s, _) in self.final.items()}
        digest("checkout/tree.sha256", final)
        dump("checkout/exit.json", self.exit)
        for a in self.agents:
            tree = dict(final)
            tree.update(self.tree_differ[a])
            tree.update(self.tree_extra[a])
            for p in self.tree_missing[a]:
                tree.pop(p, None)
            # the collector's exclusion rule must hold: control state never counts
            tree[".flint/publish.ack"] = H("control")
            digest(f"agents/{a}/tree.sha256", tree)
            lines(f"agents/{a}/conflicts.jsonl", self.conflicts[a])
        for a, js in self.journal.items():
            lines(f"agents/{a}/journal.jsonl", js)
        dump("bucket/manifest.json", {"seq": 99, "entries": {
            p: {"key": f"{PREFIX}/files/{p}", "etag": e, "crc64_b64": "AAAA", "size": 10}
            for p, (_, e) in self.final.items()}})
        heads = {p: {"etag": f'"{e}"'} for p, (_, e) in self.final.items()}
        for p, v in self.heads_override.items():
            if v == "DELETE":
                heads.pop(p, None)
            else:
                heads[p] = v
        dump("bucket/heads.json", heads)
        dump("bucket/preserved.json", self.preserved)
        dump("bucket/listing.json", [])
        for a, evs in self.traces.items():
            lines(f"traces/{a}.jsonl", evs, prose=["flint-sync: a prose line the parser must skip"])
        for node, pts in self.chrony.items():
            lines(f"nodes/{node}/chrony.jsonl", [{"ts_ms": t, "offset_ms": o} for t, o in pts])


def req(total):
    """barrier_end.requests as BUILT: {get,head,put,copy,delete,list,multipart}."""
    return {"get": total - 3, "head": 1, "put": 1, "copy": 0, "delete": 0, "list": 1, "multipart": 0}


def clean():
    g = Leg()
    A1, B1, A2, B2, B3, U1, B4, A3 = (H(x) for x in ("A1", "B1", "A2", "B2", "B3", "U1", "B4", "A3"))
    g.op("a1", 1, 1000, "write", "hot/p00.txt", "a1-1", A1)
    g.ack("a1", 1400, "a1-1", "ok", 10)
    g.op("a1", 2, 2000, "write", "hot/p02.txt", "a1-2", A2)
    g.ack("a1", 2400, "a1-2", "ok", 12)
    g.op("a1", 3, 3100, "mv", "hot/p01.txt", "a1-3", B1, base=B1, to="hot/p03.txt")
    g.ack("a1", 3500, "a1-3", "ok", 15)
    g.op("a1", 4, 6000, "write", "hot/p06.txt", "a1-4", A3)
    g.ack("a1", 21000, "a1-4", "no-ack")
    g.op("a2", 1, 1100, "write", "hot/p01.txt", "a2-1", B1)
    g.ack("a2", 1500, "a2-1", "ok", 11)
    g.op("a2", 2, 2050, "write", "hot/p02.txt", "a2-2", B2)            # concurrent with a1-2
    g.ack("a2", 2600, "a2-2", "ok", 13)
    g.op("a2", 3, 3000, "write", "hot/p00.txt", "a2-3", B3, base=A1)   # sequential over a1-1
    g.ack("a2", 3400, "a2-3", "ok", 14)
    g.op("a2", 4, 5000, "write", "hot/p05.txt", "a2-4", B4)
    g.ack("a2", 5400, "a2-4", "partial", 16, dropped=["hot/p05.txt"])
    g.op("ui", 1, 4000, "write", "hot/p04.txt", "ui-1", U1)
    g.ack("ui", 4100, "ui-1", "ok", etag='"e-u1"')
    g.final = {"hot/p00.txt": (B3, "e-b3"), "hot/p02.txt": (B2, "e-b2"),
               "hot/p03.txt": (B1, "e-b1"), "hot/p04.txt": (U1, "e-u1")}
    g.preserved = [{"key": ckey("u1", "hot/p02.txt"), "etag": '"e-a2"', "sha256": A2}]
    g.conflicts["a2"].append({"path": "hot/p02.txt", "foreign_etag": '"e-a2"',
                              "preserved_key": ckey("u1", "hot/p02.txt"), "kind": "upload-412-preserved",
                              "at_unix": 2})
    g.conflicts["a1"].append({"path": "hot/p09.txt", "foreign_etag": '"x"', "preserved_key": None,
                              "kind": "gc-skip", "at_unix": 3})
    for a, base in (("a1", 0), ("a2", 50)):
        g.ev(a, 1200 + base, "barrier_start", source="sentinel", declared=True)
        g.ev(a, 1250 + base, "claim", verdict="waiting", behind="h-other", quiet_polls=0, waited_ms=50)
        g.ev(a, 1300 + base, "claim", verdict="claimed", how="fresh", epoch=2)
        g.ev(a, 1340 + base, "merge", flush=f"f-{a}-1", theirs_seq=9, upserts=1, deletes=0, foreign=1, gone=0,
             adds_nothing=False)
        g.ev(a, 1350 + base, "cas", flush=f"f-{a}-1", seq=10, expected="m9", etag="m10", result="ok")
        g.ev(a, 1360 + base, "release", epoch=2, waiters_at_claim=1)
        g.ev(a, 1390 + base, "barrier_end", seq=10, uploaded=1, deleted=0, parked=0, consumed=0, no_change=False,
             ms=190, requests=req(7))
        g.ev(a, 1400 + base, "an_event_from_the_future", novel=True)
        total = 50
        g.ev(a, 90000 + base, "barrier_end", seq=16, uploaded=0, deleted=0, parked=0, consumed=0, no_change=True,
             ms=20, requests=req(total))
        for ts in (105000, 110000, 115000):
            total += 4
            g.ev(a, ts + base, "barrier_end", seq=16, uploaded=0, deleted=0, parked=0, consumed=0, no_change=True,
                 ms=20, requests=req(total))
    return g


# ---------- scenario mutations ------------------------------------------------

def f_lost_write(g):
    g.op("a1", 5, 7000, "write", "hot/p07.txt", "a1-5", H("A9"))
    g.ack("a1", 7400, "a1-5", "ok", 17)


def f_later_base_earlier_seq(g):
    X, Y = H("X8"), H("Y8")
    g.op("a1", 5, 8000, "write", "hot/p08.txt", "a1-5", X)
    g.ack("a1", 8400, "a1-5", "ok", 20)
    g.op("a2", 5, 8100, "write", "hot/p08.txt", "a2-5", Y, base=X)
    g.ack("a2", 8500, "a2-5", "ok", 19)
    g.final["hot/p08.txt"] = (Y, "e-y8")


def f_head_null(g):
    g.heads_override["hot/p00.txt"] = {"etag": None}


def f_head_mismatch(g):
    g.heads_override["hot/p02.txt"] = {"etag": '"e-other"'}


def f_head_missing(g):
    g.heads_override["hot/p03.txt"] = "DELETE"


def f_checkout_exit(g):
    g.exit = {"code": 1, "stderr_tail": "crc mismatch hot/p00.txt"}


def f_tree_diverged(g):
    g.tree_differ["a2"]["hot/p00.txt"] = H("stale")


def f_tree_extra(g):
    g.tree_extra["a1"]["hot/zz.txt"] = H("zz")


def f_tree_missing(g):
    g.tree_missing["a2"].add("hot/p04.txt")


def f_preserved_no_record(g):
    g.conflicts["a2"] = []


def f_record_missing_key(g):
    g.conflicts["a1"].append({"path": "hot/p00.txt", "foreign_etag": '"e-q"',
                              "preserved_key": ckey("u9", "hot/p00.txt"), "kind": "consume-dirty", "at_unix": 4})


PRIOR = {"holder": "h-a1", "epoch": 3, "released": False, "handoff": False, "waiters": 0}


def f_deadline(g):
    g.ev("a1", 50000, "claim", verdict="deadline", behind="h-a2", waited_ms=30000)
    g.ev("a1", 52000, "claim", verdict="claimed", how="fresh", epoch=4)


def f_deposed(g):
    # the deposal as BUILT: a successful claim, how=deposed, with the prior holder
    g.ev("a2", 50000, "claim", verdict="claimed", how="deposed", epoch=5, prior=PRIOR)


def nf_old_deposed_shape(g):
    # the README's first draft shape, which the syncer never emits: NOT a deposal
    g.ev("a2", 50000, "claim", verdict="deposed", epoch=3, cell_holder="h-a1", released=False, handoff=False,
         waiters=0)


def nf_orphaned_own(g):
    g.ev("a2", 50000, "claim", verdict="claimed", how="orphaned-own", epoch=6, prior=dict(PRIOR, holder="h-a2"))


def f_deadline_unrecovered(g):
    g.ev("a1", 99000, "claim", verdict="deadline", behind="h-a2", waited_ms=30000)


def f_no_contention(g):
    for a in g.traces:
        g.traces[a] = [e for e in g.traces[a] if not (e["ev"] == "claim" and e["verdict"] == "waiting")]


def f_idle_cas(g):
    g.ev("a2", 150000, "cas", flush="f-a2-150", seq=17, expected="m16", etag="m17", result="ok")


def nf_idle_cas_lost(g):
    g.ev("a2", 150000, "cas", flush="f-a2-150", seq=17, expected="m16", result="lost")


def f_idle_claimed(g):
    g.ev("a1", 150000, "claim", verdict="claimed", how="fresh", epoch=9)


def f_idle_requests(g):
    g.ev("a1", 120000, "barrier_end", seq=16, uploaded=0, deleted=0, parked=0, consumed=0, no_change=True, ms=20,
         requests=req(80))


def f_idle_requests_null(g):
    for e in g.traces["a1"]:
        if e["ev"] == "barrier_end" and e["ts_ms"] >= 90000:
            e["requests"] = None


def f_covered_later_then_lost(g):
    g.op("a1", 5, 9000, "write", "hot/p09.txt", "a1-5", H("C9"))
    g.ack("a1", 24000, "a1-5", "no-ack")
    g.op("a1", 6, 24500, "write", "hot/p10.txt", "a1-6", H("D10"))
    g.ack("a1", 25000, "a1-6", "ok", 21, covered=["a1-5", "a1-6"])
    g.final["hot/p10.txt"] = (H("D10"), "e-d10")


def f_ui_lost(g):
    g.op("ui", 2, 9800, "write", "hot/p11.txt", "ui-2", H("U2"))
    g.ack("ui", 9900, "ui-2", "ok", etag='"e-u2"')


def f_mv_destination_lost(g):
    F = H("F14")
    g.op("a1", 5, 10000, "mv", "hot/p14.txt", "a1-5", F, base=F, to="hot/p15.txt")
    g.ack("a1", 10400, "a1-5", "ok", 23)


def nf_sequential(g, shut=False):
    S1, S2 = H("S1"), H("S2")
    g.op("a1", 5, 20000, "write", "hot/p20.txt", "a1-5", S1)
    g.ack("a1", 20400, "a1-5", "ok", 30)
    g.op("a2", 5, 20500, "write", "hot/p20.txt", "a2-5", S2, base="absent" if shut else S1)
    g.ack("a2", 20900, "a2-5", "ok", 31)
    g.final["hot/p20.txt"] = (S2, "e-s2")


def nf_concurrent_preserved(g, shut=False, other_path=False):
    R1, R2 = H("R1"), H("R2")
    g.op("a1", 5, 21000, "write", "hot/p21.txt", "a1-5", R1)
    g.ack("a1", 21400, "a1-5", "ok", 32)
    g.op("a2", 5, 21050, "write", "hot/p21.txt", "a2-5", R2)
    g.ack("a2", 21600, "a2-5", "ok", 33)
    g.final["hot/p21.txt"] = (R2, "e-r2")
    if not shut:
        path = "hot/p22.txt" if other_path else "hot/p21.txt"
        g.preserved.append({"key": ckey("u2", path), "etag": '"e-r1"', "sha256": R1})
        g.conflicts["a2"].append({"path": path, "foreign_etag": '"e-r1"', "preserved_key": ckey("u2", path),
                                  "kind": "upload-412-preserved", "at_unix": 5})


def nf_coalesced_same_seq(g):
    M1, M2 = H("M1"), H("M2")
    g.op("a1", 5, 23000, "write", "hot/p23.txt", "a1-40", M1)
    g.ack("a1", 38000, "a1-40", "no-ack")
    g.op("a1", 6, 38100, "write", "hot/p23.txt", "a1-41", M2, base=M1)
    g.ack("a1", 38600, "a1-41", "ok", 34, covered=["a1-40", "a1-41"])
    g.final["hot/p23.txt"] = (M2, "e-m2")


def nf_ui_supersedes(g, ui_t=24600):
    T1, T2 = H("T1"), H("T2")
    g.op("a1", 5, 24000, "write", "hot/p24.txt", "a1-5", T1)
    g.ack("a1", 24400, "a1-5", "ok", 35)
    g.op("ui", 2, ui_t, "write", "hot/p24.txt", "ui-2", T2, base=T1)
    g.ack("ui", ui_t + 100, "ui-2", "ok", etag='"e-t2"')
    g.final["hot/p24.txt"] = (T2, "e-t2")


def nf_partial_dropped(g, shut=False):
    g.op("a1", 5, 25000, "write", "hot/p25.txt", "a1-5", H("P1"))
    if shut:
        g.ack("a1", 25400, "a1-5", "ok", 36)
    else:
        g.ack("a1", 25400, "a1-5", "partial", 36, dropped=["hot/p25.txt"])


def nf_mv_source(g, shut=False):
    Y1 = H("Y1")
    g.op("a1", 5, 26000, "write", "hot/p26.txt", "a1-5", Y1)
    g.ack("a1", 26400, "a1-5", "ok", 37)
    g.op("a2", 5, 26500, "mv", "hot/p26.txt", "a2-5", Y1, base=H("other") if shut else Y1, to="hot/p27.txt")
    g.ack("a2", 26900, "a2-5", "ok", 38)
    g.final["hot/p27.txt"] = (Y1, "e-y1")


def nf_ui_behind_clock(g, chrony=True):
    # the UI's node runs 1000 ms BEHIND: its op (true 28300) is stamped
    # 27300, before the agent's op at 28000. Only the correction orders it.
    W1, W2 = H("W1"), H("W2")
    g.meta["agent_nodes"] = {"a1": "n1", "a2": "n2", "ui": "n-ui"}
    if chrony:
        g.chrony = {"n1": [(0, 0)], "n2": [(0, 0)], "n-ui": [(0, -1000)]}
    g.op("a1", 5, 28000, "write", "hot/p28.txt", "a1-5", W1)
    g.ack("a1", 28400, "a1-5", "ok", 39)
    g.op("ui", 2, 27300, "write", "hot/p28.txt", "ui-2", W2, base=W1)
    g.ack("ui", 27400, "ui-2", "ok", etag='"e-w2"')
    g.final["hot/p28.txt"] = (W2, "e-w2")


def with_args(fn, *extra):
    def m(g):
        fn(g)
        g.args += list(extra)
    return m


SCENARIOS = [
    # name, kind, what, mutation, expected failing oracles
    ("clean", "clean", "every oracle passes", lambda g: None, set()),
    ("a-lost-acked-write", "fault", "acked write, not final, no copy, no later base", f_lost_write, {"O3"}),
    ("a-later-base-earlier-seq", "fault", "base==h only on an op acked at a LOWER seq", f_later_base_earlier_seq, {"O3"}),
    ("a-mv-destination-lost", "fault", "a mv's `to` content is gone", f_mv_destination_lost, {"O3"}),
    ("a-ui-write-lost", "fault", "a UI write acked by the gateway, gone", f_ui_lost, {"O3"}),
    ("a-covered-later-then-lost", "fault", "no-ack batch covered by a later ack, content gone",
     f_covered_later_then_lost, {"O3"}),
    ("b-head-etag-null", "fault", "a citation's HEAD finds nothing", f_head_null, {"O1"}),
    ("b-head-etag-mismatch", "fault", "a citation's HEAD etag differs", f_head_mismatch, {"O1"}),
    ("b-citation-not-headed", "fault", "a citation missing from heads.json", f_head_missing, {"O1"}),
    ("c-checkout-exit-nonzero", "fault", "fresh checkout failed", f_checkout_exit, {"O1"}),
    ("d-tree-diverged", "fault", "one agent tree has a different sha", f_tree_diverged, {"O2"}),
    ("e-tree-extra-file", "fault", "one agent tree has an extra file", f_tree_extra, {"O2"}),
    ("f-tree-missing-file", "fault", "one agent tree lacks a file", f_tree_missing, {"O2"}),
    ("g-preserved-without-record", "fault", "a preserved object nobody recorded", f_preserved_no_record, {"O4"}),
    ("g-record-names-missing-key", "fault", "a record naming a key not in the bucket", f_record_missing_key, {"O4"}),
    ("h-claim-deadline", "fault", "deadline on a no-fault leg", f_deadline, {"O5"}),
    ("h-claim-deposed", "fault", "deposal (claimed, how=deposed) on a no-fault leg", f_deposed, {"O5"},
     lambda v: v["oracles"]["O5"]["details"]["deposed"] == 1),
    ("h-old-deposed-shape", "non-fault", "verdict=deposed is not a shape the syncer emits: not counted",
     nf_old_deposed_shape, set(), lambda v: v["oracles"]["O5"]["details"]["deposed"] == 0),
    ("h-orphaned-own", "non-fault", "claimed how=orphaned-own: reported, not failed", nf_orphaned_own, set(),
     lambda v: v["oracles"]["O5"]["details"]["orphaned_own"] == 1),
    ("h-deposal-declared", "non-fault", "--faults-declared: a deposal is expected",
     with_args(f_deposed, "--faults-declared"), set()),
    ("h-deadline-declared-unrecovered", "fault", "--faults-declared, no claim after the deadline",
     with_args(f_deadline_unrecovered, "--faults-declared"), {"O5"}),
    ("h-no-contention", "fault", "no claim ever waited", f_no_contention, {"O5"}),
    ("i-idle-cas-ok", "fault", "a cas ok inside the idle window", f_idle_cas, {"O6"}),
    ("i-idle-claimed", "fault", "a claim claimed inside the idle window", f_idle_claimed, {"O6"}),
    ("i-idle-over-baseline", "fault", "an idle tick over the request baseline", f_idle_requests, {"O6"}),
    ("i-idle-requests-null", "fault", "baseline given, requests null: nothing measured", f_idle_requests_null,
     {"O6"}),
    ("i-idle-cas-lost", "non-fault", "a cas LOST inside the idle window is not an install", nf_idle_cas_lost, set()),
    ("h-deadline-declared-recovered", "non-fault", "--faults-declared, deadline then claimed",
     with_args(f_deadline, "--faults-declared"), set()),
    ("nf-sequential-edit", "non-fault", "B's base == A's acked h (superseded arm only)", nf_sequential, set()),
    ("nf-sequential-edit/ctl", "control", "same, B's base shut", lambda g: nf_sequential(g, shut=True), {"O3"}),
    ("nf-concurrent-preserved", "non-fault", "concurrent edit, loser preserved (preserved arm only)",
     nf_concurrent_preserved, set()),
    ("nf-concurrent-preserved/ctl", "control", "same, copy + record removed",
     lambda g: nf_concurrent_preserved(g, shut=True), {"O3"}),
    ("nf-concurrent-preserved/ctl-path", "control", "same, copy is of ANOTHER path",
     lambda g: nf_concurrent_preserved(g, other_path=True), {"O3"}),
    ("nf-coalesced-same-seq", "non-fault", "two batches, one ack: later base at EQUAL seq", nf_coalesced_same_seq, set()),
    ("nf-ui-supersedes", "non-fault", "UI read the agent's h and replaced it (wall-time arm)", nf_ui_supersedes, set()),
    ("nf-ui-supersedes/ctl", "control", "same, UI op journaled before the agent's op",
     lambda g: nf_ui_supersedes(g, ui_t=23900), {"O3"}),
    ("nf-ui-behind-clock", "non-fault", "UI node 1 s behind, chrony corrects the order", nf_ui_behind_clock, set()),
    ("nf-ui-behind-clock/ctl", "control", "same, no chrony samples", lambda g: nf_ui_behind_clock(g, chrony=False),
     {"O3"}),
    ("nf-partial-dropped", "non-fault", "partial ack dropping the path: not acked", nf_partial_dropped, set()),
    ("nf-partial-dropped/ctl", "control", "same, status ok", lambda g: nf_partial_dropped(g, shut=True), {"O3"}),
    ("nf-mv-source", "non-fault", "a mv whose base is the acked h", nf_mv_source, set()),
    ("nf-mv-source/ctl", "control", "same, mv base differs", lambda g: nf_mv_source(g, shut=True), {"O3"}),
]


FIXTURE = os.path.join(HERE, "testdata", "trace-two-writers.jsonl")
FLUSH_EVENTS = {"scan", "upload", "observed", "merge", "cas", "gc", "drill_hold", "queue"}
README_EVENTS = {"barrier_start", "consume", "tombstone", "scan", "upload", "claim", "observed", "merge", "cas",
                 "gc", "queue", "release", "fence", "drill_hold", "barrier_end", "ack", "sync"}


def fixture_checks(base):
    """The REAL syncer trace (testdata/trace-two-writers.jsonl) through
    timeline.py and oracle.py exactly as a collected leg would be read: one
    trace file per holder, named by holder id."""
    out = []

    def check(name, ok, detail=""):
        out.append((name, bool(ok), detail))

    if not os.path.exists(FIXTURE):
        check("fixture present", False, FIXTURE)
        return out
    raw = [json.loads(l) for l in open(FIXTURE) if l.strip()]
    root = os.path.join(base, "real-trace-fixture")
    shutil.rmtree(root, ignore_errors=True)
    os.makedirs(os.path.join(root, "traces"))
    with open(os.path.join(root, "meta.json"), "w") as f:
        json.dump({"leg": "real-trace-fixture", "agents": []}, f)
    holders = []
    for e in raw:
        if e.get("holder") not in holders:
            holders.append(e.get("holder"))
    for h in holders:
        with open(os.path.join(root, "traces", f"{h}.jsonl"), "w") as f:
            f.write("flint-sync: prose between trace lines\n")
            for e in raw:
                if e.get("holder") == h:
                    f.write(json.dumps(e, separators=(",", ":")) + "\n")
    check(f"fixture: {len(raw)} lines, {len(holders)} holders, every line names its holder",
          len(raw) == 49 and len(holders) == 2 and None not in holders, (len(raw), holders))
    check("fixture: common prefix ts_ms, mono_ms, holder, ev on every line",
          all(all(k in e for k in ("ts_ms", "mono_ms", "holder", "ev")) for e in raw))
    stray = sorted({e["ev"] for e in raw if "flush" in e and e["ev"] not in FLUSH_EVENTS})
    check("fixture: flush only on the events README §2 names", not stray, stray)
    undocumented = sorted({e["ev"] for e in raw} - README_EVENTS)
    check("fixture: every event is documented in README §2", not undocumented, undocumented)

    tl = subprocess.run([sys.executable, os.path.join(HERE, "timeline.py"), root, "--json"],
                        capture_output=True, text=True)
    try:
        rows = json.loads(tl.stdout)
    except ValueError:
        check("timeline: --json over the fixture", False, tl.stderr[-300:])
        return out
    ev = [r["event"] for r in rows]
    check("timeline: all 49 events merged, none dropped", len(ev) == 49, len(ev))
    ts = [r["t_ms"] for r in rows]
    check("timeline: merged order is non-decreasing in time across the two files", ts == sorted(ts))
    ix = lambda pred: next((i for i, e in enumerate(ev) if pred(e)), None)
    gc = ix(lambda e: e["ev"] == "gc" and e.get("path") == "x.txt" and e.get("result") == "deleted")
    tomb = ix(lambda e: e["ev"] == "tombstone" and e.get("path") == "x.txt" and e.get("action") == "removed")
    check("timeline: gc deleted x.txt is ordered before the other holder's tombstone removed x.txt",
          gc is not None and tomb is not None and gc < tomb and ev[gc]["holder"] != ev[tomb]["holder"], (gc, tomb))
    tlp = subprocess.run([sys.executable, os.path.join(HERE, "timeline.py"), root, "--json", "--path", "x.txt",
                          "--context"], capture_output=True, text=True)
    ctx = [r["event"] for r in json.loads(tlp.stdout or "[]")]
    named = [e for e in ctx if e.get("path")]
    gc_holder = ev[gc]["holder"] if gc is not None else None
    check("timeline: --path x.txt --context = upload, gc (of that etag), tombstone, plus the gc barrier's "
          "claim, cas and end",
          [(e["ev"], e.get("path")) for e in named] == [("upload", "x.txt"), ("gc", "x.txt"), ("tombstone", "x.txt")]
          and named[0].get("etag") == named[1].get("head")
          and any(e["ev"] == "claim" and e["holder"] == gc_holder for e in ctx)
          and any(e["ev"] == "cas" and e["holder"] == gc_holder for e in ctx)
          and any(e["ev"] == "barrier_end" and e["holder"] == gc_holder and e.get("deleted") == 1 for e in ctx),
          [(e["ev"], e.get("path")) for e in ctx])

    res, problems = run_one(root, ["--oracles", "O5"], all_six=False)
    if res is None:
        check("oracle O5 over the fixture", False, problems)
        return out
    d = res[1]["oracles"]["O5"]["details"]
    check("O5: 0 deposals, 0 deadlines, 0 orphaned-own on the real trace",
          d["deposed"] == 0 and d["deadline"] == 0 and d["orphaned_own"] == 0, d)
    claimed = sum(v for c in d["claims_by_trace"].values() for k, v in c.items() if k.startswith("claimed:"))
    check("O5: reads all 5 real claims (verdict claimed, with how)", claimed == 5, d["claims_by_trace"])
    check("O5: fails ONLY as vacuous (the fixture has no waiting claim)",
          res[0] == {"O5"} and len(d["reasons"]) == 1 and "no claim ever waited" in d["reasons"][0]
          and not d["problems"], d["reasons"])
    last_claim = max(e["ts_ms"] for e in raw if e["ev"] == "claim")
    last_cas = max(e["ts_ms"] for e in raw if e["ev"] == "cas")
    end = max(e["ts_ms"] for e in raw)
    quiet = run_one(root, ["--oracles", "O6", "--idle-from", str(last_claim + 1), "--idle-to", str(end),
                           "--request-baseline", "1000"], all_six=False)[0]
    check("O6: the fixture's tail after its last claim is quiet", quiet is not None and quiet[0] == set(),
          quiet and quiet[1]["oracles"]["O6"]["details"]["reasons"])
    busy = run_one(root, ["--oracles", "O6", "--idle-from", str(last_cas - 1), "--idle-to", str(end),
                          "--request-baseline", "1000"], all_six=False)[0]
    rs = busy[1]["oracles"]["O6"]["details"]["reasons"] if busy else []
    check("O6: a window over the last cas ok and claim fails on both",
          busy is not None and busy[0] == {"O6"} and any("cas ok" in r for r in rs)
          and any("claimed" in r for r in rs), rs)
    return out


def run_one(root, args, all_six=True):
    p = subprocess.run([sys.executable, ORACLE, root] + args, capture_output=True, text=True)
    try:
        verdict = json.loads(p.stdout)
    except ValueError:
        return None, f"oracle printed no JSON (rc={p.returncode}): {p.stderr.strip()[-300:]}"
    problems = []
    if set(verdict.get("oracles", {})) != {"O1", "O2", "O3", "O4", "O5", "O6"}:
        problems.append("verdict does not name all six oracles")
    if (p.returncode == 0) != bool(verdict.get("pass")):
        problems.append(f"exit code {p.returncode} disagrees with pass={verdict.get('pass')}")
    unevaluated = sorted(o for o, r in verdict["oracles"].items() if r.get("pass") is None)
    if unevaluated and all_six:
        problems.append(f"not evaluated: {unevaluated}")
    failing = {o for o, r in verdict["oracles"].items() if r.get("pass") is False}
    for o in failing:
        if not verdict["oracles"][o].get("details", {}).get("reasons"):
            problems.append(f"{o} failed without a reason")
    return (failing, verdict), "; ".join(problems)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--keep")
    ap.add_argument("--only")
    a = ap.parse_args()
    only = set(a.only.split(",")) if a.only else None
    base = a.keep or tempfile.mkdtemp(prefix="oracle-selftest-")
    rows, bad = [], 0
    for name, kind, what, mutate, expected, *probe in SCENARIOS:
        if only and name not in only:
            continue
        g = clean()
        mutate(g)
        root = os.path.join(base, name.replace("/", "_"))
        shutil.rmtree(root, ignore_errors=True)
        g.write(root)
        res, problems = run_one(root, g.args)
        if res is None:
            ok, got = False, "?"
        else:
            failing, verdict = res
            if probe and not probe[0](verdict):
                problems = (problems + "; " if problems else "") + "detail probe failed"
            ok = failing == expected and not problems
            got = ",".join(sorted(failing)) or "-"
        bad += 0 if ok else 1
        rows.append((name, kind, ",".join(sorted(expected)) or "-", got, "PASS" if ok else "FAIL",
                     what + (f"  !! {problems}" if problems else "")))
    w = [max(len(r[i]) for r in rows + [("scenario", "kind", "expect", "got", "result", "")]) for i in range(5)]
    print(f"{'scenario':<{w[0]}}  {'kind':<{w[1]}}  {'expect':<{w[2]}}  {'got':<{w[3]}}  {'result':<{w[4]}}  what")
    for r in rows:
        print(f"{r[0]:<{w[0]}}  {r[1]:<{w[1]}}  {r[2]:<{w[2]}}  {r[3]:<{w[3]}}  {r[4]:<{w[4]}}  {r[5]}")
    print("\nreal syncer trace (testdata/trace-two-writers.jsonl):")
    fx = fixture_checks(base)
    for name, ok, detail in fx:
        print(f"  {'PASS' if ok else 'FAIL'}  {name}" + ("" if ok else f"  {detail}"))
    fx_bad = sum(1 for _, ok, _ in fx if not ok)
    print(f"\noracle selftest: {len(rows) - bad}/{len(rows)} scenarios as expected, "
          f"{len(fx) - fx_bad}/{len(fx)} fixture checks" + ("" if not (bad or fx_bad) else " — FAILED"))
    bad += fx_bad
    if not a.keep:
        shutil.rmtree(base, ignore_errors=True)
    return 0 if bad == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
