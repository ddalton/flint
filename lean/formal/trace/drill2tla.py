#!/usr/bin/env python3
"""Project a LIVE DRILL leg onto one path, as the NDJSON ndjson2tla.py reads.

    drill2tla.py <collect-dir> --path hot/p07.txt [-o out.ndjson]

W4 phase 2. Phase 1 replayed the conformance harness, which LOGS what the
syncer's trace cannot see (the agent's writes, the UI's writes, the touch).
A storm leg logs the same things in other files:

  agents/<agent>/journal.jsonl   every op the agent performed, with sha256
  agents/ui/journal.jsonl        the UI's writes, and the ETAG the gateway
                                 returned for each
  traces/<agent>.jsonl           the syncer's protocol event trace

so nothing here is inferred: the agent's writes are read, not guessed.

WHAT PROJECTION MEANS. The model's world is the paths it is given. A leg
touches 30-50 shared paths; replaying one path drops every event about the
others. Three consequences, all of them made explicit rather than hidden:

  * counts the trace reports over ALL paths (a scan's uploads/deletes, a
    merge's foreign/gone) cannot be recomputed for the projection, so the
    step omits them and `TraceLean.tla` checks them only when present;
  * an ack's status likewise speaks for the whole barrier, so a projected
    ack accepts either AckOk or AckPartial;
  * everything path-scoped is checked exactly as phase 1 checks it: what a
    consume adopted and removed, each upload's outcome and generation,
    which citations the commit withheld, each GC's result, the manifest
    sequence, and the order of all of it.

A leg with kills is refused: a pod replacement is a crash and a fresh
checkout, which this projector does not emit yet.

Generations. The model identifies a version by the generation it mints;
the drill identifies it by an etag (the content's hash) in the trace and a
sha256 in the journal. Each distinct (path, sha256) an agent writes mints
one generation, and the etag the syncer reports for that writer's next
upload of that path binds to it. A UI write's etag comes from its own ack.
An etag no rule binds is a refusal, not a guess.
"""
import argparse
import json
import sys
from pathlib import Path

STRUCTURAL = {"barrier_start", "scan", "claim", "merge", "cas", "window_clear",
              "barrier_end", "ack", "queue", "sweep", "release", "handed_off", "fence", "sync"}
PATHED = {"consume", "tombstone", "upload", "observed", "gc"}


class Refuse(Exception):
    pass


def load_jsonl(p):
    out = []
    if not p.exists():
        return out
    for line in p.read_text(errors="replace").splitlines():
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            out.append(json.loads(line))
        except ValueError:
            pass
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("collect")
    ap.add_argument("--path", required=True, help="the one path to project onto")
    ap.add_argument("-o", "--out", default="-")
    a = ap.parse_args()
    root = Path(a.collect)
    P = a.path

    meta = json.loads((root / "meta.json").read_text())
    if any((n.get("kills") or 0) for n in meta.get("nodes") or []):
        raise Refuse("this leg declares kills: a replacement is a crash and a fresh checkout, "
                     "which this projector does not emit")

    # ── the syncer traces, and which holder is which agent ──────────────
    events = []          # (ts, seq_in_file, agent, event)
    holder_of = {}
    for tr in sorted((root / "traces").glob("*.jsonl")):
        agent = tr.stem
        for n, e in enumerate(load_jsonl(tr)):
            if e.get("holder"):
                holder_of.setdefault(agent, e["holder"])
            events.append((e["ts_ms"], n, agent, e))
    agents = sorted({a for _, _, a, _ in events})
    if not agents:
        raise Refuse("no traces in this collect")
    if len(agents) > 6:
        raise Refuse(f"{len(agents)} writers; the model's Writers tops out at six")

    # ── the agents' own ops, and the UI's, from the journals ────────────
    ui_etag = {}         # nonce -> etag (the gateway's answer)
    for j in load_jsonl(root / "agents" / "ui" / "journal.jsonl"):
        if j.get("k") == "ack" and j.get("status") == "ok" and j.get("etag"):
            ui_etag[j["nonce"]] = j["etag"].strip('"')
    ops = []             # (ts, kind, agent, path, sha|None, nonce)
    for d in sorted((root / "agents").glob("*")):
        if not d.is_dir():
            continue
        agent = d.name
        for j in load_jsonl(d / "journal.jsonl"):
            if j.get("k") != "op":
                continue
            kind, path, to = j.get("op"), j.get("path"), j.get("to")
            if kind == "write" and path == P:
                ops.append((j["t_ms"], "write", agent, P, j.get("sha256"), j.get("nonce")))
            elif kind == "delete" and path == P:
                ops.append((j["t_ms"], "delete", agent, P, None, j.get("nonce")))
            elif kind == "mv":
                if path == P:
                    ops.append((j["t_ms"], "delete", agent, P, None, j.get("nonce")))
                if to == P:
                    ops.append((j["t_ms"], "write", agent, P, j.get("sha256"), j.get("nonce")))
    if not ops:
        raise Refuse(f"no agent or UI op touched {P} in this leg")

    # ── generations: one per distinct (path, sha256) an agent wrote ─────
    gen_of_sha = {}
    next_gen = [1]

    def gen_for(sha):
        if sha not in gen_of_sha:
            gen_of_sha[sha] = next_gen[0]
            next_gen[0] += 1
        return gen_of_sha[sha]

    # ── merge, in time order; a UI op is 'ui', everything else a writer ──
    merged = []
    for ts, kind, agent, path, sha, nonce in ops:
        merged.append((ts, 0, "op", (kind, agent, path, sha, nonce)))
    for ts, n, agent, e in events:
        ev = e["ev"]
        if ev in PATHED and e.get("path") != P:
            continue
        if ev not in STRUCTURAL and ev not in PATHED:
            continue
        merged.append((ts, 1 + n, "ev", (agent, e)))
    # ── order by CAUSALITY where the trace carries it ───────────────────
    # Three nodes, three clocks: a merge by wall time alone puts a claim
    # before the release it actually followed. The cell's own numbering is
    # the order that matters and the trace carries it — every claim names
    # its epoch, every install its manifest seq — so a claim of epoch N is
    # held until the release of epoch N-1 has been emitted, and a CAS of
    # seq S until the CAS of seq S-1. Everything else keeps its writer's
    # own order, and ties break on the timestamp.
    per_writer = {}
    for item in merged:
        key = item[3][1] if item[2] == "ev" else ("op", item[3][1])
        who = item[3][0] if item[2] == "ev" else item[3][1]
        per_writer.setdefault(who, []).append(item)
    for q in per_writer.values():
        q.sort(key=lambda x: (x[0], x[1]))
    heads = {w: 0 for w in per_writer}
    released_epoch = 0          # the highest epoch whose release was emitted
    cas_seq = 0                 # the highest manifest seq installed
    ordered = []
    stalled_rounds = 0
    while any(heads[w] < len(per_writer[w]) for w in per_writer):
        ready = []
        for w, q in per_writer.items():
            if heads[w] >= len(q):
                continue
            item = q[heads[w]]
            if item[2] == "ev":
                e = item[3][1]
                if e["ev"] == "claim" and e.get("verdict") == "claimed" and isinstance(e.get("epoch"), int):
                    if e["epoch"] - 1 > released_epoch and stalled_rounds < 2:
                        continue
                if e["ev"] == "cas" and e.get("result") == "ok" and isinstance(e.get("seq"), int):
                    if e["seq"] - 1 > cas_seq and stalled_rounds < 2:
                        continue
            ready.append((item[0], item[1], w))
        if not ready:
            stalled_rounds += 1          # a gap the projection cannot close: fall back to time
            if stalled_rounds > 3:
                break
            continue
        stalled_rounds = 0
        _, _, w = min(ready)
        item = per_writer[w][heads[w]]
        heads[w] += 1
        if item[2] == "ev":
            e = item[3][1]
            if e["ev"] in ("release", "handed_off") and isinstance(e.get("epoch"), int):
                released_epoch = max(released_epoch, e["epoch"])
            if e["ev"] == "cas" and e.get("result") == "ok" and isinstance(e.get("seq"), int):
                cas_seq = max(cas_seq, e["seq"])
        ordered.append(item)
    for w, q in per_writer.items():        # anything the scheduler could not place
        ordered.extend(q[heads[w]:])
    merged = ordered

    # ── emit ────────────────────────────────────────────────────────────
    letters = {}
    out = [{"ts_ms": 0, "ev": "conf_projected", "path": P}]
    started = set()
    pending_write = {}   # agent -> sha waiting for the etag of its upload
    etag_gen = {}        # etag -> generation

    def writer(agent):
        if agent not in letters:
            letters[agent] = "ABCDEF"[len(letters)]
        return letters[agent]

    def start(agent, ts):
        if agent in started:
            return
        started.add(agent)
        out.append({"ts_ms": ts, "ev": "conf_start", "writer": writer(agent),
                    "holder": holder_of.get(agent, agent), "entries": {}, "seq": 1})

    for ts, _, what, payload in merged:
        if what == "op":
            kind, agent, path, sha, nonce = payload
            if agent == "ui":
                if nonce not in ui_etag:
                    continue          # refused, or answered with an error: nothing landed for sure
                et = ui_etag[nonce]
                etag_gen.setdefault(et, gen_for(sha))
                out.append({"ts_ms": ts, "ev": "conf_hitl_write", "path": P, "etag": et})
                continue
            start(agent, ts)
            if kind == "write":
                out.append({"ts_ms": ts, "ev": "conf_agent_write", "writer": writer(agent),
                            "path": P, "etag": f"sha:{sha}", "sha256": sha})
                pending_write[agent] = sha
            else:
                out.append({"ts_ms": ts, "ev": "conf_agent_delete", "writer": writer(agent), "path": P})
            continue
        agent, e = payload
        ev = e["ev"]
        start(agent, ts)
        e = dict(e)
        e["holder"] = holder_of.get(agent, e.get("holder"))
        if ev == "upload" and e.get("etag"):
            et = e["etag"].strip('"')
            sha = pending_write.pop(agent, None)
            if sha is not None:
                etag_gen.setdefault(et, gen_for(sha))
            e["etag"] = et
        for k in ("etag",):
            if isinstance(e.get(k), str):
                e[k] = e[k].strip('"')
        if ev == "scan":
            e.pop("uploads", None)
            e.pop("deletes", None)          # a whole-leg count; the projection cannot recompute it
        if ev == "merge":
            e.pop("foreign", None)
            e.pop("gone", None)
        if ev == "ack":
            e["status"] = "projected"       # the barrier's status speaks for every path
        out.append(e)

    # Only the etags the converter READS have to be named: a consume that
    # adopted or preserved bytes, and an upload that put or adopted them.
    # A parked upload's etag is the foreign version it did not take, and an
    # `observed` line's is only echoed back, so neither needs a generation.
    def reads_etag(e):
        return ((e.get("ev") == "consume" and e.get("action") in ("adopted", "dirty-preserved"))
                or (e.get("ev") == "upload" and e.get("outcome") in ("put", "adopted"))
                # a UI write MINTS a generation, and its etag is the name the
                # consumes that adopt it carry
                or e.get("ev") == "conf_hitl_write")

    unknown = set()
    for e in out:
        if reads_etag(e) and e.get("etag") and not e["etag"].startswith("sha:") and e["etag"] not in etag_gen:
            unknown.add(e["etag"])
    if unknown:
        raise Refuse(f"{len(unknown)} etag(s) no write of {P} accounts for "
                     f"(a version written before this leg's trace, or by a killed writer): "
                     f"{sorted(unknown)[:3]}")
    # Rewrite every etag as the generation's stable name, so ndjson2tla.py's
    # (path, etag) -> generation map is the one built here.
    sha_of_gen = {g: sh for sh, g in gen_of_sha.items()}
    for e in out:
        et = e.get("etag")
        if isinstance(et, str) and reads_etag(e) and not et.startswith("sha:"):
            e["etag"] = f"sha:{sha_of_gen[etag_gen[et]]}"

    # The barrier the trace does not mark. A barrier that fails part way
    # through — the store refuses a request (S3 answered the window-open PUT
    # with 409 ConditionalRequestConflict in this leg), an upload errors —
    # returns the error and emits NO `window_clear` and no `barrier_end`;
    # the syncer says so only in prose. Two places show it, and both are
    # read here as one `conf_abandon`:
    #   * a holder RELEASES the cell with no window_clear since its claim —
    #     the cell goes back at that moment, so the step belongs there;
    #   * a holder STARTS a barrier while its previous one never ended.
    state = {}          # holder -> {"open": bool, "claimed": bool, "done": bool}
    marks = []
    for i, e in enumerate(out):
        h, ev = e.get("holder"), e.get("ev")
        if not h or ev not in ("barrier_start", "barrier_end", "claim", "window_clear", "release"):
            continue
        st = state.setdefault(h, {"open": False, "claimed": False, "done": False})
        if ev == "barrier_start":
            if st["open"] and not st["done"]:
                marks.append((i, h))
            state[h] = {"open": True, "claimed": False, "done": False}
        elif ev == "barrier_end":
            st["open"] = False
        elif ev == "claim" and e.get("verdict") == "claimed":
            st["claimed"] = True
        elif ev == "window_clear":
            st["claimed"] = False          # the commit completed; the release is ordinary
        elif ev == "release" and st["claimed"]:
            marks.append((i, h))
            st["claimed"] = False
            st["done"] = True
    inserts = []
    for i, h in marks:
        w = next(l["writer"] for l in out if l.get("ev") == "conf_start" and l.get("holder") == h)
        inserts.append((i, {"ts_ms": out[i]["ts_ms"], "ev": "conf_abandon", "writer": w}))
    for j, rec in sorted(inserts, reverse=True):
        out.insert(j, rec)

    # The sentinel touch the trace does not carry. An `ack` event is the
    # answer to a `.flint/publish` the agent wrote before that barrier; the
    # model needs the Touch and the TakeSentinel that precede it, so each
    # ack's barrier gets one, immediately before its barrier_start.
    touched = []
    for i, e in enumerate(out):
        if e.get("ev") != "ack":
            continue
        holder = e.get("holder")
        j = max((k for k in range(i) if out[k].get("ev") == "barrier_start"
                 and out[k].get("holder") == holder), default=None)
        if j is None:
            raise Refuse("an ack with no barrier_start before it")
        w = next(l["writer"] for l in out if l.get("ev") == "conf_start" and l.get("holder") == holder)
        touched.append((j, {"ts_ms": out[j]["ts_ms"], "ev": "conf_touch", "writer": w}))
    for j, rec in sorted(touched, reverse=True):
        out.insert(j, rec)

    text = "\n".join(json.dumps(e) for e in out) + "\n"
    if a.out == "-":
        sys.stdout.write(text)
    else:
        Path(a.out).write_text(text)
    writers = ", ".join(f"{k}={v}" for k, v in letters.items())
    print(f"{root.name} {P}: {len(out)} lines, {len(letters)} writer(s) [{writers}], "
          f"{len(gen_of_sha)} generation(s)", file=sys.stderr)
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Refuse as e:
        print(f"REFUSED: {e}", file=sys.stderr)
        sys.exit(2)
