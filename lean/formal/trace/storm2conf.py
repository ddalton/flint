#!/usr/bin/env python3
"""storm2conf.py -- a multi-writer run, as ONE conformance trace.

    storm2conf.py <run-dir> <out.ndjson>

A real run (lean/e2e/writers-live/storm-local.sh, or a storm leg) leaves
each writer's event trace (w<i>/sync.log, its `{"ts_ms":` lines) and its
agent's journal (w<i>/agent/journal.jsonl). No single log orders them:
each writer logged alone. This script writes the NDJSON a conformance
scenario would have written for the same run -- `conf_start` per writer,
the agents' ops as `conf_agent_write` / `conf_agent_delete`, the writers'
events as they are -- in ONE order that the shared state forces, so that
ndjson2core.py and TraceCore.tla check it unchanged.

THE ORDER. Within a writer, its events keep their order, and its agent's
ops go in by time (agent and syncer share the host's clock). Across
writers only what they SHARED orders them, never a wall clock:

  * a consume block -- `derive` (the seq the consume read) to the scan or
    `barrier_end` that closes it, the model's one Consume step -- comes
    after the install of that seq and closes before the next install;
  * a merge (`theirs_seq`) likewise: after that install, before the next;
  * a lease claim of epoch e comes after the release of epoch e - 1.

Among the events these leave free, the earliest timestamp goes first (any
choice is a linearisation the constraints allow; the free ones commute in
the model -- a writer's local steps). Constraints that cannot all hold are
a contradiction in the run itself, reported, never forced.

THE JOIN. An agent's write is named by the MD5 of its bytes (agent.sh's
`md5`), a plain PUT's etag on S3 and MinIO: so `conf_agent_write`'s etag is
the one every upload, consume and surface of those bytes logs.

What it does not do yet (refused, with the reason): the UI actor (the
gateway logs no seq), a writer that restarted (a second `start`), a move
(agent mode churn), and a write with no md5 (an agent.sh before it).
"""
import heapq
import json
import sys
from pathlib import Path


class Refused(Exception):
    pass


def load_writer(d: Path):
    evs = []
    for line in (d / "sync.log").read_text(errors="replace").splitlines():
        if line.startswith('{"ts_ms":'):
            evs.append(json.loads(line))
    holders = {e["holder"] for e in evs if e.get("holder")}
    if len(holders) != 1:
        raise Refused(f"{d.name}: {len(holders)} holders (a restart or replacement is not supported yet)")
    starts = [e for e in evs if e["ev"] == "start"]
    if len(starts) != 1:
        raise Refused(f"{d.name}: {len(starts)} starts (a restart is not supported yet)")
    ops = []
    j = d / "agent" / "journal.jsonl"
    if j.exists():
        for line in j.read_text().splitlines():
            o = json.loads(line)
            if o.get("k") != "op":
                continue
            if o["op"] == "mv":
                raise Refused(f"{d.name}: an agent move (op {o['n']}): not supported yet")
            if o["op"] == "write" and not o.get("md5"):
                raise Refused(f"{d.name}: agent write {o['n']} has no md5 (agent.sh before the join)")
            ops.append(o)
    return holders.pop(), evs, ops


def writer_stream(name, holder, evs, ops):
    """The writer's events and its agent's ops, in one list of
    (ts_ms, record). The incarnation's `start` and the store probe are
    dropped: the checkout is conf_start's step."""
    out = []
    for k, e in enumerate(evs):
        if e["ev"] in ("start", "conformance"):
            continue
        out.append((e["ts_ms"], 0, k, e))
    for o in ops:
        if o["op"] == "write":
            r = {"ev": "conf_agent_write", "writer": name, "path": o["path"], "etag": f'"{o["md5"]}"'}
        else:
            r = {"ev": "conf_agent_delete", "writer": name, "path": o["path"]}
        r["ts_ms"] = o["t_ms"]
        out.append((o["t_ms"], 1, o["n"], r))
    # A tie: the syncer's event first (the agent's op is stamped BEFORE its
    # rename lands, so a scan in the same ms cannot have read it).
    out.sort(key=lambda x: (x[0], x[1], x[2]))
    return [r for _, _, _, r in out]


def annotate(stream):
    """Each event's cross-writer needs: `after_install` (a seq whose
    install must already be out), `reads` (a seq whose NEXT install must
    wait for this event), `install` (the seq it installs), `claims` /
    `releases` (lease epochs)."""
    ann = [dict() for _ in stream]
    open_derive = None  # index of the derive opening the current consume block
    in_block = False
    for i, e in enumerate(stream):
        ev = e["ev"]
        if ev == "barrier_start":
            in_block, open_derive = True, None
        elif ev == "derive" and in_block:
            open_derive = i
            ann[i]["after_install"] = e["seq"]
        elif ev in ("scan", "barrier_end") and in_block:
            # The block's close: the model's Consume step is taken here, so
            # the document it read must still be current.
            seq = stream[open_derive]["seq"] if open_derive is not None else 0
            ann[i]["reads"] = seq
            in_block = False
        elif ev == "merge":
            ann[i]["after_install"] = e["theirs_seq"]
            ann[i]["reads"] = e["theirs_seq"]
        elif ev == "cas":
            if e["result"] != "ok":
                raise Refused("a lost CAS")
            ann[i]["install"] = e["seq"]
        elif ev == "claim" and e["verdict"] == "claimed":
            ann[i]["claims"] = e["epoch"]
        elif ev == "release":
            ann[i]["releases"] = e["epoch"]
    return ann


def merge(streams):
    names = sorted(streams)
    anns = {n: annotate(streams[n]) for n in names}
    # Every event that reads seq k: install k+1 waits for all of them.
    readers_of = {}
    for n in names:
        for i, a in enumerate(anns[n]):
            if "reads" in a:
                readers_of.setdefault(a["reads"], set()).add((n, i))
    installed = {0}  # seq 0: the empty workspace, before any install
    released = set()
    first_epoch = min((a["claims"] for n in names for a in anns[n] if "claims" in a), default=1)
    done = set()
    pos = {n: 0 for n in names}
    out = []

    def ready(n, i):
        a = anns[n][i]
        if "after_install" in a and a["after_install"] not in installed:
            return False
        if "install" in a:
            k = a["install"]
            if k - 1 not in installed:
                return False
            if not readers_of.get(k - 1, set()) <= done:
                # Its own merge read k - 1 and is already out; a peer's
                # consume that read k - 1 must close first.
                return False
        if "claims" in a and a["claims"] != first_epoch and a["claims"] - 1 not in released:
            return False
        return True

    while True:
        cand = []
        for n in names:
            i = pos[n]
            if i < len(streams[n]) and ready(n, i):
                heapq.heappush(cand, (streams[n][i]["ts_ms"], n, i))
        if not cand:
            break
        _, n, i = heapq.heappop(cand)
        a = anns[n][i]
        out.append(streams[n][i])
        done.add((n, i))
        pos[n] += 1
        if "install" in a:
            installed.add(a["install"])
        if "releases" in a:
            released.add(a["releases"])
    stuck = {n: streams[n][pos[n]] for n in names if pos[n] < len(streams[n])}
    if stuck:
        lines = [f"  {n} at {e['ev']} {json.dumps({k: e[k] for k in e if k in ('seq', 'theirs_seq', 'epoch', 'path')})}"
                 for n, e in stuck.items()]
        raise Refused("the shared order is contradictory -- no writer can go on:\n" + "\n".join(lines))
    return out


def main():
    run, dst = Path(sys.argv[1]), Path(sys.argv[2])
    try:
        if (run / "ui").exists():
            raise Refused("a UI actor (the gateway logs no seq): not supported yet")
        streams, holders = {}, {}
        for d in sorted(run.glob("w*")):
            if not (d / "sync.log").exists():
                continue
            holder, evs, ops = load_writer(d)
            holders[d.name] = holder
            streams[d.name] = writer_stream(d.name, holder, evs, ops)
        out = merge(streams)
    except Refused as e:
        print(f"REFUSED {run.name}: {e}")
        return 2
    with dst.open("w") as f:
        # Every writer checked out the empty workspace before any agent ran
        # (storm-local.sh starts the agents after the last checkout).
        for n in sorted(holders):
            f.write(json.dumps({"ev": "conf_start", "holder": holders[n], "writer": n,
                                "entries": {}, "seq": 0, "keys": {}}) + "\n")
        for e in out:
            f.write(json.dumps(e) + "\n")
    print(f"{run.name}: {len(streams)} writers, {len(out)} events -> {dst}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
