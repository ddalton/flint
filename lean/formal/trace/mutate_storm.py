#!/usr/bin/env python3
"""Corrupt a MERGED multi-writer trace (storm2conf.py's output) so the core
trace check must reject it: what a replay of several writers defends that
one writer's scenario cannot -- a peer's version reaching every tree, the
order of a consume against a peer's install, one holder in the commit
section. Each mutation edits ONE fact; a checker that accepted any of them
would be checking less than it says.

  mutate_storm.py <merged.ndjson> <out-dir>   writes <out-dir>/<name>.ndjson
"""
import json
import sys
from pathlib import Path


def find(evs, pred, what):
    for i, e in enumerate(evs):
        if pred(i, e):
            return i
    raise SystemExit(f"mutation anchor not found: {what}")


def writer_of(evs):
    return {e["holder"]: e["writer"] for e in evs if e["ev"] == "conf_start"}


def written_by(evs):
    """(path, etag) -> the writer whose agent wrote those bytes"""
    return {(e["path"], e["etag"]): e["writer"] for e in evs if e["ev"] == "conf_agent_write"}


def peer_version_never_adopted(evs):
    """a tree reports it never took a version a PEER published"""
    wo, wb = writer_of(evs), written_by(evs)
    i = find(evs, lambda i, e: e["ev"] == "consume" and e["action"] == "adopted"
             and wb.get((e["path"], e["etag"])) not in (None, wo[e["holder"]]), "a peer's version adopted")
    del evs[i]


def adopts_a_superseded_version(evs):
    """a consume reports the version of the path BEFORE the one the document cited"""
    wo = writer_of(evs)
    seen = {}  # path -> etags in the order the agents wrote them
    for e in evs:
        if e["ev"] == "conf_agent_write":
            seen.setdefault(e["path"], []).append(e["etag"])
    i = find(evs, lambda i, e: e["ev"] == "consume" and e["action"] == "adopted"
             and e["etag"] in seen.get(e["path"], [])[1:], "an adoption of a path's second or later version")
    vs = seen[evs[i]["path"]]
    evs[i]["etag"] = vs[vs.index(evs[i]["etag"]) - 1]


def upload_names_a_peers_bytes(evs):
    """an upload reports the bytes a peer wrote at that path, not its own agent's"""
    wo, wb = writer_of(evs), written_by(evs)
    peer = {}
    for (p, et), w in wb.items():
        peer.setdefault(p, []).append((w, et))
    i = find(evs, lambda i, e: e["ev"] == "upload" and e.get("outcome") == "put"
             and any(w != wo[e["holder"]] for w, _ in peer.get(e["path"], [])), "an upload of a path a peer wrote too")
    e = evs[i]
    e["etag"] = next(et for w, et in peer[e["path"]] if w != wo[e["holder"]])


def cas_seq_skips(evs):
    """a CAS reports a seq one past the one it installed"""
    i = find(evs, lambda i, e: e["ev"] == "cas" and e["result"] == "ok" and e["seq"] > 2, "a CAS after the second")
    evs[i]["seq"] += 1


def merge_foreign_plus_one(evs):
    """a merge reports one more foreign change than the document carried"""
    i = find(evs, lambda i, e: e["ev"] == "merge" and e["foreign"] > 0, "a merge with foreign changes")
    evs[i]["foreign"] += 1


def install_inside_a_peers_consume(evs):
    """a peer's install lands between a consume's read and its close (the consume adopts the older document)"""
    wo = writer_of(evs)
    # a consume block that adopted something, closed by a scan; the next
    # install by ANOTHER writer moves to just before the block's close
    for i, e in enumerate(evs):
        if e["ev"] != "consume" or e["action"] != "adopted":
            continue
        w = wo[e["holder"]]
        close = next((j for j in range(i + 1, len(evs))
                      if evs[j].get("holder") == e["holder"] and evs[j]["ev"] in ("scan", "barrier_end")), None)
        if close is None:
            continue
        cas = next((j for j in range(close + 1, len(evs))
                    if evs[j]["ev"] == "cas" and wo.get(evs[j].get("holder")) != w), None)
        if cas is None:
            continue
        # The peer's whole commit section moves: its claim .. window_clear.
        h = evs[cas]["holder"]
        a = max(j for j in range(close) if evs[j].get("holder") == h and evs[j]["ev"] == "barrier_start") \
            if any(evs[j].get("holder") == h and evs[j]["ev"] == "barrier_start" for j in range(close)) else None
        claim = next((j for j in range(close + 1, cas) if evs[j].get("holder") == h
                      and evs[j]["ev"] == "claim" and evs[j]["verdict"] == "claimed"), None)
        end = next((j for j in range(cas, len(evs)) if evs[j].get("holder") == h and evs[j]["ev"] == "window_clear"), None)
        if claim is None or end is None or a is None:
            continue
        block = [evs[j] for j in range(claim, end + 1) if evs[j].get("holder") == h]
        rest = [evs[j] for j in range(len(evs)) if not (claim <= j <= end and evs[j].get("holder") == h)]
        k = rest.index(evs[close])
        evs[:] = rest[:k] + block + rest[k:]
        return
    raise SystemExit("mutation anchor not found: a consume block a peer's later install could move into")


def two_holders(evs):
    """a writer claims the lease before the holder before it cleared its window"""
    wo = writer_of(evs)
    claims = [i for i, e in enumerate(evs) if e["ev"] == "claim" and e["verdict"] == "claimed"]
    for i in claims[1:]:
        h = evs[i]["holder"]
        clear = max((j for j in range(i) if evs[j]["ev"] == "window_clear" and evs[j]["holder"] != h), default=None)
        if clear is None:
            continue
        cas = max((j for j in range(clear) if evs[j]["ev"] == "cas" and evs[j]["holder"] == evs[clear]["holder"]), default=None)
        if cas is None:
            continue
        e = evs.pop(i)
        evs.insert(cas, e)  # the claim now precedes the previous holder's CAS
        return
    raise SystemExit("mutation anchor not found: two consecutive holders")


MUTATIONS = [peer_version_never_adopted, adopts_a_superseded_version, upload_names_a_peers_bytes,
             cas_seq_skips, merge_foreign_plus_one, install_inside_a_peers_consume, two_holders]


def main():
    src, out = Path(sys.argv[1]), Path(sys.argv[2])
    out.mkdir(parents=True, exist_ok=True)
    base = [json.loads(l) for l in src.read_text().splitlines() if l.strip()]
    for m in MUTATIONS:
        evs = [dict(e) for e in base]
        m(evs)
        name = m.__name__.replace("_", "-")
        (out / f"{name}.ndjson").write_text("".join(json.dumps(e) + "\n" for e in evs))
        print(f"{name}: {m.__doc__}")


if __name__ == "__main__":
    main()
