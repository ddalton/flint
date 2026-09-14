#!/usr/bin/env python3
"""contention_analyze.py — judge contention.sh runs from their event traces.

    contention_analyze.py RUN_DIR_OR_TGZ [...]   per-run table, then per-arm ranges
    contention_analyze.py --json ...             the same as JSON lines

Per run, from the writers' `FLINT_SYNC_EVENT_TRACE` lines and the agents'
journals:

- barriers, claims taken, pull-only barriers (a merge with no upserts and no
  deletes) and how many of those claimed the cell;
- claim wait (first claim event of a barrier -> its `claimed`);
- hold (`claimed` -> `release`) and its parts: claimed->merge, merge->cas,
  cas->window_clear, window_clear->generation sweep, ->chunk sweep, ->release;
- handoff latency (`release` -> `handed_off`) and the reserved waiter's delay
  (`handed_off` -> the next `claimed`);
- the fraction of the load phase the cell was held;
- publish acks (ok / partial / no-ack) and ack latency (the batch's last op ->
  the agent's ack line);
- requests per writer per floor tick in the idle phase, from the deltas of
  `barrier_end`'s cumulative per-process request counts.

Quantiles are nearest-rank; an empty series prints "-". Runs are grouped by
the `arm` in meta.json and each metric's p50 is shown as a range across runs,
never as a mean of means.
"""
import glob
import json
import os
import sys
import tarfile
import tempfile


def pct(xs, p):
    xs = sorted(xs)
    if not xs:
        return None
    return xs[min(len(xs) - 1, int(p / 100 * len(xs)))]


def read_jsonl(path, prefix='{"ts_ms"'):
    out = []
    try:
        with open(path, errors="replace") as f:
            for line in f:
                if not line.startswith(prefix):
                    continue
                try:
                    out.append(json.loads(line))
                except ValueError:
                    pass
    except FileNotFoundError:
        pass
    return out


def load_run(path):
    if path.endswith(".tgz"):
        tmp = tempfile.mkdtemp(prefix="contention-")
        with tarfile.open(path) as t:
            t.extractall(tmp)
        # macOS tar adds AppleDouble `._*` members; the run is the one directory.
        (name,) = [n for n in os.listdir(tmp) if not n.startswith("._") and os.path.isdir(os.path.join(tmp, n))]
        path = os.path.join(tmp, name)
    meta = json.load(open(os.path.join(path, "meta.json")))
    phases = {d["ev"]: d["ts_ms"] for d in read_jsonl(os.path.join(path, "phases.jsonl"), prefix="{")}
    verdict = {}
    if os.path.exists(os.path.join(path, "verdict.json")):
        verdict = json.load(open(os.path.join(path, "verdict.json")))
    elif os.path.exists(os.path.join(path, "verdict.txt")):
        verdict = {"pass": False, "reasons": [open(os.path.join(path, "verdict.txt")).read().strip()]}
    writers = sorted(glob.glob(os.path.join(path, "w*")))
    traces = {os.path.basename(w): read_jsonl(os.path.join(w, "sync.log")) for w in writers}
    journals = {os.path.basename(w): read_jsonl(os.path.join(w, "agent", "journal.jsonl"), prefix="{") for w in writers}
    return path, meta, phases, verdict, traces, journals


def analyse(path):
    path, meta, phases, verdict, traces, journals = load_run(path)
    load0, load1 = phases.get("load_start"), phases.get("load_end")
    idle1 = phases.get("idle_end")
    floor_ms = meta["floor"] * 1000

    barriers = []
    for w, evs in traces.items():
        evs.sort(key=lambda d: (d["ts_ms"], d.get("mono_ms", 0)))
        cur = None
        prev_total = {}
        for d in evs:
            e = d["ev"]
            if e == "barrier_start":
                cur = {"w": w, "start": d["ts_ms"], "source": d.get("source"), "claims": [], "sweeps": [], "work": False}
            elif cur is None:
                continue
            elif e == "claim":
                cur["claims"].append((d["ts_ms"], d.get("verdict")))
                if d.get("how") == "deposed":
                    cur["deposed"] = True
            elif e in ("consume", "tombstone", "scan", "upload"):
                cur["work"] = True
            elif e == "merge":
                cur.setdefault("merge", d["ts_ms"])
                cur["pull"] = d.get("upserts") == 0 and d.get("deletes") == 0
            elif e == "cas" and d.get("result") == "ok":
                cur["cas"] = d["ts_ms"]
            elif e == "window_clear":
                cur["window_clear"] = d["ts_ms"]
            elif e == "sweep":
                cur["sweeps"].append((d["ts_ms"], d.get("what")))
            elif e == "release":
                cur["release"] = d["ts_ms"]
            elif e == "handed_off":
                cur["handed_off"] = d["ts_ms"]
            elif e == "barrier_end":
                cur["end"] = d["ts_ms"]
                # `requests` is the store's CUMULATIVE count for the process:
                # a barrier's own requests are the delta from the previous one.
                total = d.get("requests") or {}
                cur["requests"] = {k: v - prev_total.get(k, 0) for k, v in total.items()}
                prev_total = total
                cur["no_change"] = d.get("no_change")
                cur["uploaded"] = d.get("uploaded")
                barriers.append(cur)
                cur = None

    claimed = []
    for b in barriers:
        t = [ts for ts, v in b["claims"] if v == "claimed"]
        if t and "release" in b:
            b["claimed"] = t[0]
            claimed.append(b)

    def series(f):
        out = []
        for b in claimed:
            try:
                v = f(b)
            except (KeyError, TypeError, IndexError):
                continue
            if v is not None:
                out.append(v)
        return out

    in_load = [b for b in claimed if load0 and load1 and load0 <= b["claimed"] <= load1 + floor_ms]
    r = {
        "run": os.path.basename(path.rstrip("/")),
        "arm": meta["arm"],
        "writers": meta["writers"],
        "pass": verdict.get("pass"),
        "reasons": verdict.get("reasons"),
        "barriers": len(barriers),
        "claims": len(claimed),
        "pull_only_barriers": sum(1 for b in barriers if b.get("pull")),
        "pull_only_claims": sum(1 for b in claimed if b.get("pull")),
        # A release that no `handed_off` followed: the cell was left held.
        "lost_handoffs": sum(1 for b in barriers if "release" in b and "handed_off" not in b),
        "deposals": sum(1 for b in barriers if b.get("deposed")),
    }
    r["claim_wait_ms"] = series(lambda b: b["claimed"] - b["claims"][0][0])
    r["hold_ms"] = series(lambda b: b["release"] - b["claimed"])
    r["hold_publishing_ms"] = series(lambda b: (b["release"] - b["claimed"]) if not b.get("pull") else None)
    r["claimed_to_merge_ms"] = series(lambda b: b["merge"] - b["claimed"])
    r["merge_to_cas_ms"] = series(lambda b: b["cas"] - b["merge"])
    r["cas_to_window_clear_ms"] = series(lambda b: b["window_clear"] - b["cas"])
    r["window_clear_to_gen_sweep_ms"] = series(
        lambda b: [t for t, w in b["sweeps"] if w == "generations"][0] - b["window_clear"])
    r["gen_sweep_to_chunk_sweep_ms"] = series(
        lambda b: [t for t, w in b["sweeps"] if w == "chunks"][0] - [t for t, w in b["sweeps"] if w == "generations"][0])
    r["sweeps_to_release_ms"] = series(lambda b: b["release"] - b["sweeps"][-1][0])
    r["handoff_request_ms"] = series(lambda b: b["handed_off"] - b["release"])

    # The cell across writers: the next claim after each handoff.
    ordered = sorted(claimed, key=lambda b: b["claimed"])
    gaps_rel, gaps_ho = [], []
    for prev, nxt in zip(ordered, ordered[1:]):
        if nxt["claimed"] < prev["release"]:
            continue
        if nxt["claims"][0][0] > prev["release"]:
            continue  # nobody was waiting when it was released: not a handoff delay
        gaps_rel.append(nxt["claimed"] - prev["release"])
        if "handed_off" in prev:
            gaps_ho.append(nxt["claimed"] - prev["handed_off"])
    r["release_to_next_claim_ms"] = gaps_rel
    r["handed_off_to_next_claim_ms"] = gaps_ho
    if load0 and load1 and in_load:
        held = sum(min(b["release"], load1) - max(b["claimed"], load0) for b in in_load if b["release"] > load0)
        r["cell_held_fraction"] = round(held / (load1 - load0), 3)

    acks, lat = {"ok": 0, "partial": 0, "no-ack": 0, "other": 0}, []
    for w, js in journals.items():
        last_op = {}
        for d in js:
            if d.get("k") == "op" and d.get("nonce"):
                last_op[d["nonce"]] = max(last_op.get(d["nonce"], 0), d.get("t_ms", 0))
            elif d.get("k") == "ack":
                s = d.get("status")
                acks[s if s in acks else "other"] += 1
                if s in ("ok", "partial") and d.get("nonce") in last_op:
                    lat.append(d["t_ms"] - last_op[d["nonce"]])
    r["acks"] = acks
    r["ack_latency_ms"] = lat

    # Idle: whole floor ticks after the agents stopped (one floor of slack).
    if load1 and idle1:
        t0, t1 = load1 + 2 * floor_ms, idle1
        window = [b for b in barriers if t0 <= b["start"] <= t1 and b.get("source") == "cadence"]
        # Only the ticks with nothing to do: a writer still consuming its
        # peers' last changes is converging, not idle.
        idle = [b for b in window if not b["work"] and "merge" not in b]
        r["catch_up_ticks"] = len(window) - len(idle)
        reqs = {}
        for b in idle:
            for k, v in b["requests"].items():
                reqs[k] = reqs.get(k, 0) + v
        ticks = len(idle)
        r["idle_ticks"] = ticks
        r["idle_requests_per_tick"] = {k: round(v / ticks, 2) for k, v in reqs.items() if v} if ticks else {}
    load_reqs = {}
    for b in barriers:
        if load0 and load1 and load0 <= b["start"] <= load1:
            for k, v in b["requests"].items():
                load_reqs[k] = load_reqs.get(k, 0) + v
    r["load_requests"] = load_reqs
    return r


def fmt(xs):
    if not xs:
        return "-"
    return f"p50 {pct(xs, 50)} p90 {pct(xs, 90)} n {len(xs)}"


def main(argv):
    as_json = "--json" in argv
    paths = [a for a in argv if a != "--json"]
    runs = [analyse(p) for p in paths]
    if as_json:
        for r in runs:
            print(json.dumps(r))
        return
    series_keys = ["claim_wait_ms", "hold_ms", "hold_publishing_ms", "claimed_to_merge_ms", "merge_to_cas_ms",
                   "cas_to_window_clear_ms", "window_clear_to_gen_sweep_ms", "gen_sweep_to_chunk_sweep_ms",
                   "sweeps_to_release_ms", "handoff_request_ms", "release_to_next_claim_ms",
                   "handed_off_to_next_claim_ms", "ack_latency_ms"]
    for r in runs:
        print(f"== {r['run']}  arm {r['arm']}  writers {r['writers']}  pass {r['pass']} {r['reasons'] or ''}")
        print(f"   barriers {r['barriers']}  claims {r['claims']}  pull-only barriers {r['pull_only_barriers']}"
              f" (claimed {r['pull_only_claims']})  cell held {r.get('cell_held_fraction')}"
              f"  lost handoffs {r['lost_handoffs']}  deposals {r['deposals']}")
        for k in series_keys:
            print(f"   {k:30s} {fmt(r[k])}")
        print(f"   acks {r['acks']}")
        print(f"   idle ticks {r.get('idle_ticks')} (+{r.get('catch_up_ticks')} catch-up)  requests/tick {r.get('idle_requests_per_tick')}")
        print(f"   load requests {r['load_requests']}")
    arms = {}
    for r in runs:
        arms.setdefault(r["arm"], []).append(r)
    print("\n== per arm: p50 across runs [min..max], counts summed")
    for arm, rs in sorted(arms.items()):
        print(f"-- arm {arm}: {len(rs)} run(s), pass {[r['pass'] for r in rs]}")
        for k in ["claim_wait_ms", "hold_ms", "hold_publishing_ms", "release_to_next_claim_ms",
                  "handoff_request_ms", "handed_off_to_next_claim_ms", "cas_to_window_clear_ms",
                  "window_clear_to_gen_sweep_ms", "ack_latency_ms"]:
            p50s = [pct(r[k], 50) for r in rs if r[k]]
            p90s = [pct(r[k], 90) for r in rs if r[k]]
            if p50s:
                print(f"   {k:30s} p50 [{min(p50s)}..{max(p50s)}]  p90 [{min(p90s)}..{max(p90s)}]")
        print(f"   claims {[r['claims'] for r in rs]}  pull-only claims {[r['pull_only_claims'] for r in rs]}"
              f"  cell held {[r.get('cell_held_fraction') for r in rs]}")
        print(f"   no-ack {[r['acks']['no-ack'] for r in rs]} of acks {[sum(r['acks'].values()) for r in rs]}")
        print(f"   lost handoffs {[r['lost_handoffs'] for r in rs]}  deposals {[r['deposals'] for r in rs]}")
        print(f"   idle requests/tick {[r.get('idle_requests_per_tick') for r in rs]}")


if __name__ == "__main__":
    main(sys.argv[1:])
