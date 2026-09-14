#!/usr/bin/env python3
"""timeline.py — one chronological table of a collected leg's evidence.

    timeline.py <collect/leg> [--path P] [--from MS] [--to MS]
                [--journals] [--context] [--no-correct] [--json]
    timeline.py --selftest

Reads every `traces/<agent>.jsonl` (only lines starting `{"ts_ms":`; the
rest of a worker's stderr is prose and is skipped), and with --journals
every `agents/<agent>/journal.jsonl` line carrying `t_ms`.

Clock correction (README §5): `meta.json` may carry
`agent_nodes: {"<agent>": "<node>", "ui": "<node>"}`; `nodes/<node>/chrony.jsonl`
carries `{ts_ms, offset_ms}` samples where offset_ms = node clock − true
time (positive: the node runs AHEAD; chronyc's "N seconds fast of NTP time"
is +N×1000, "slow" is −N×1000). An event stamped `ts` on that node is placed
at `ts − offset(ts)`, the offset linearly interpolated between the samples
around `ts` (clamped at the ends). A source with no node or no samples is
left uncorrected and marked `-` in the skew column.

--path P keeps events whose `path` or `to` equals P (a glob if P contains
* ? or [). --context adds, for those events, the path-less events of the
same barrier — in the same trace between the enclosing `barrier_start` and
`barrier_end` (claim, cas, merge, release…; `flush` is on only some events,
so it is a secondary key) — and the journal `ack` of a matched op. Unknown
events and fields are shown as they are, never rejected.
Times are the corrected epoch ms; --from/--to filter on them.
"""
import argparse
import bisect
import fnmatch
import glob
import json
import os
import sys
import tempfile
import time

TRACE_PREFIX = '{"ts_ms":'
COMMON = ("ts_ms", "mono_ms", "holder", "flush", "ev", "path", "to", "k", "agent", "t_ms")


def load_meta(leg):
    try:
        with open(os.path.join(leg, "meta.json")) as f:
            return json.load(f)
    except (FileNotFoundError, ValueError):
        return {}


def load_traces(leg, problems=None):
    """{source: [event, …]} in file (emission) order; source = file stem."""
    out = {}
    for path in sorted(glob.glob(os.path.join(leg, "traces", "*.jsonl"))):
        src = os.path.splitext(os.path.basename(path))[0]
        evs = []
        with open(path, errors="replace") as f:
            for i, line in enumerate(f, 1):
                if not line.startswith(TRACE_PREFIX):
                    continue
                try:
                    obj = json.loads(line)
                except ValueError:
                    if problems is not None:
                        problems.append(f"{path}:{i}: trace line is not JSON")
                    continue
                if not isinstance(obj.get("ts_ms"), (int, float)):
                    if problems is not None:
                        problems.append(f"{path}:{i}: ts_ms is not a number")
                    continue
                obj["_i"] = i
                evs.append(obj)
        out[src] = evs
    return out


def load_journals(leg, problems=None):
    """{source: [line, …]} for agents/<source>/journal.jsonl, in file order."""
    out = {}
    for path in sorted(glob.glob(os.path.join(leg, "agents", "*", "journal.jsonl"))):
        src = os.path.basename(os.path.dirname(path))
        lines = []
        with open(path, errors="replace") as f:
            for i, line in enumerate(f, 1):
                if not line.strip():
                    continue
                try:
                    obj = json.loads(line)
                except ValueError:
                    if problems is not None:
                        problems.append(f"{path}:{i}: journal line is not JSON")
                    continue
                obj["_i"] = i
                lines.append(obj)
        out[src] = lines
    return out


class Clock:
    """Per-source offset correction from chrony samples (see module doc)."""

    def __init__(self, leg, enabled=True):
        self.enabled = enabled
        self.agent_nodes = (load_meta(leg).get("agent_nodes") or {}) if enabled else {}
        self.samples = {}
        if not enabled:
            return
        for node in set(self.agent_nodes.values()):
            path = os.path.join(leg, "nodes", node, "chrony.jsonl")
            pts = []
            try:
                with open(path, errors="replace") as f:
                    for line in f:
                        try:
                            o = json.loads(line)
                            pts.append((float(o["ts_ms"]), float(o["offset_ms"])))
                        except (ValueError, KeyError, TypeError):
                            continue
            except FileNotFoundError:
                continue
            if pts:
                pts.sort()
                self.samples[node] = ([p[0] for p in pts], [p[1] for p in pts])

    def offset(self, source, ts):
        node = self.agent_nodes.get(source)
        if node is None or node not in self.samples or ts is None:
            return None
        xs, ys = self.samples[node]
        if ts <= xs[0]:
            return ys[0]
        if ts >= xs[-1]:
            return ys[-1]
        j = bisect.bisect_right(xs, ts)
        x0, x1, y0, y1 = xs[j - 1], xs[j], ys[j - 1], ys[j]
        return y0 if x1 == x0 else y0 + (y1 - y0) * (ts - x0) / (x1 - x0)

    def correct(self, source, ts):
        off = self.offset(source, ts)
        return ts if off is None else ts - off


def path_matches(obj, pat):
    globby = any(c in pat for c in "*?[")
    for key in ("path", "to"):
        v = obj.get(key)
        if isinstance(v, str) and (fnmatch.fnmatchcase(v, pat) if globby else v == pat):
            return True
    return False


def barrier_spans(evs):
    """[(first line, last line)] of each barrier in one trace, by file order;
    a barrier still open at the end of the file runs to infinity."""
    spans, open_i = [], None
    for e in evs:
        if e.get("ev") == "barrier_start":
            if open_i is not None:
                spans.append((open_i, e["_i"] - 1))
            open_i = e["_i"]
        elif e.get("ev") == "barrier_end" and open_i is not None:
            spans.append((open_i, e["_i"]))
            open_i = None
    if open_i is not None:
        spans.append((open_i, float("inf")))
    return spans


def span_of(spans, i):
    for a, b in spans:
        if a <= i <= b:
            return (a, b)
    return None


def collect(leg, journals=False, correct=True, path=None, t_from=None, t_to=None, context=False):
    clock = Clock(leg, enabled=correct)
    rows = []
    traces = load_traces(leg)
    spans = {src: barrier_spans(evs) for src, evs in traces.items()}
    for src, evs in traces.items():
        for e in evs:
            off = clock.offset(src, e["ts_ms"])
            rows.append({"t": clock.correct(src, e["ts_ms"]), "raw": e["ts_ms"], "skew": off,
                         "src": src, "kind": "trace", "i": e["_i"], "e": e})
    if journals:
        for src, lines in load_journals(leg).items():
            for o in lines:
                if not isinstance(o.get("t_ms"), (int, float)):
                    continue
                off = clock.offset(src, o["t_ms"])
                rows.append({"t": clock.correct(src, o["t_ms"]), "raw": o["t_ms"], "skew": off,
                             "src": src, "kind": "journal", "i": o["_i"], "e": o})
    rows.sort(key=lambda r: (r["t"], r["src"], r["kind"], r["i"]))
    if path is not None:
        hit = [r for r in rows if path_matches(r["e"], path)]
        if context:
            flushes = {(r["src"], r["e"].get("flush")) for r in hit
                       if r["kind"] == "trace" and r["e"].get("flush")}
            barriers = {(r["src"], span_of(spans[r["src"]], r["i"])) for r in hit if r["kind"] == "trace"}
            barriers = {b for b in barriers if b[1] is not None}
            nonces = {(r["src"], r["e"].get("nonce")) for r in hit if r["kind"] == "journal"}
            keep = set(id(r) for r in hit)
            for r in rows:
                e = r["e"]
                if r["kind"] == "trace" and not e.get("path") and (
                        (r["src"], e.get("flush")) in flushes
                        or (r["src"], span_of(spans[r["src"]], r["i"])) in barriers):
                    keep.add(id(r))
                if r["kind"] == "journal" and e.get("k") == "ack" and (r["src"], e.get("nonce")) in nonces:
                    keep.add(id(r))
            rows = [r for r in rows if id(r) in keep]
        else:
            rows = hit
    if t_from is not None:
        rows = [r for r in rows if r["t"] >= t_from]
    if t_to is not None:
        rows = [r for r in rows if r["t"] <= t_to]
    return rows


def fmt_rows(rows):
    out = []
    head = f"{'t_ms(corr)':>15} {'utc':>12} {'skew':>7} {'src':<8} {'holder':<10} {'flush':<8} {'ev':<13} {'path':<28} detail"
    out.append(head)
    for r in rows:
        e = r["e"]
        clock = time.strftime("%H:%M:%S", time.gmtime(r["t"] / 1000.0)) + f".{int(r['t']) % 1000:03d}"
        skew = "-" if r["skew"] is None else f"{r['skew']:+.0f}"
        ev = e.get("ev") if r["kind"] == "trace" else f"j:{e.get('k')}" + (f":{e.get('op')}" if e.get("op") else "")
        p = e.get("path") or ""
        if e.get("to"):
            p = f"{p} -> {e['to']}"
        detail = " ".join(f"{k}={json.dumps(v, separators=(',', ':'))}" for k, v in e.items()
                          if k not in COMMON and not k.startswith("_"))
        out.append(f"{r['t']:>15.0f} {clock:>12} {skew:>7} {r['src']:<8} {str(e.get('holder') or '')[:10]:<10} "
                   f"{str(e.get('flush') or '')[:8]:<8} {str(ev):<13} {p:<28} {detail}")
    return "\n".join(out)


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("leg", nargs="?")
    ap.add_argument("--path")
    ap.add_argument("--from", dest="t_from", type=float)
    ap.add_argument("--to", dest="t_to", type=float)
    ap.add_argument("--journals", action="store_true")
    ap.add_argument("--context", action="store_true")
    ap.add_argument("--no-correct", action="store_true")
    ap.add_argument("--json", action="store_true")
    ap.add_argument("--selftest", action="store_true")
    a = ap.parse_args(argv)
    if a.selftest:
        return selftest()
    if not a.leg or not os.path.isdir(a.leg):
        ap.error("a collected leg directory is required")
    rows = collect(a.leg, journals=a.journals, correct=not a.no_correct, path=a.path,
                   t_from=a.t_from, t_to=a.t_to, context=a.context)
    if a.json:
        print(json.dumps([{"t_ms": r["t"], "raw_ms": r["raw"], "skew_ms": r["skew"], "src": r["src"],
                           "kind": r["kind"], "event": {k: v for k, v in r["e"].items() if not k.startswith("_")}}
                          for r in rows], indent=1))
    else:
        print(fmt_rows(rows))
    return 0


# ---------------------------------------------------------------------------
# Self-test: two holders on skewed clocks, in the trace shapes as BUILT
# (README §2: `flush` only on some events, claims carry `how`). The TRUE
# interleaving is the F1 shape — A claims, B uploads x, A's GC of x, B's CAS —
# and the raw stamps order it wrongly; only the correction restores it. The
# same leg with the correction switched off must NOT restore it, or the test
# proves nothing.

def selftest():
    fails = []

    def check(name, cond, detail=""):
        print(f"  {'PASS' if cond else 'FAIL'}  {name}" + (f"  ({detail})" if detail and not cond else ""))
        if not cond:
            fails.append(name)

    with tempfile.TemporaryDirectory() as leg:
        for d in ("traces", "nodes/n1", "nodes/n2", "agents/a"):
            os.makedirs(os.path.join(leg, d))
        with open(os.path.join(leg, "meta.json"), "w") as f:
            json.dump({"leg": "timeline-selftest", "agent_nodes": {"a": "n1", "b": "n2"}}, f)
        # n1 runs ahead: 200 ms at t=0 drifting to 300 ms at t=2500, so the
        # offset at raw r is 200 + 0.04 r (interpolated). n2 runs 100 ms behind.
        with open(os.path.join(leg, "nodes", "n1", "chrony.jsonl"), "w") as f:
            f.write('{"ts_ms":0,"offset_ms":200}\n{"ts_ms":2500,"offset_ms":300}\n')
        with open(os.path.join(leg, "nodes", "n2", "chrony.jsonl"), "w") as f:
            f.write('{"ts_ms":0,"offset_ms":-100}\n')

        def ev(ts, holder, evname, **kw):
            d = {"ts_ms": ts, "mono_ms": ts - 900, "holder": holder, "ev": evname}
            d.update(kw)
            return json.dumps(d, separators=(",", ":"))

        a_raw = lambda true: round((true + 200) / 0.96)   # inverts true = r - (200 + 0.04 r)
        with open(os.path.join(leg, "traces", "a.jsonl"), "w") as f:
            f.write("flint-sync: starting (prose, skipped)\n")
            f.write(ev(a_raw(950), "hA", "barrier_start", source="sentinel", declared=True) + "\n")
            f.write(ev(a_raw(1000), "hA", "claim", verdict="claimed", how="fresh", epoch=7) + "\n")
            f.write('{"ts_ms": not json\n')
            f.write(ev(a_raw(1200), "hA", "gc", flush="fa1", path="x", head="e1", recognized=False,
                       result="skip") + "\n")
            f.write(ev(a_raw(1250), "hA", "barrier_end", seq=8, uploaded=0, deleted=1, parked=0, consumed=0,
                       no_change=False, ms=300, requests=None) + "\n")
            f.write(ev(a_raw(1400), "hA", "release", epoch=7, waiters_at_claim=1) + "\n")
            f.write(ev(a_raw(1450), "hA", "some_future_event", novel_field=[1, 2]) + "\n")
        with open(os.path.join(leg, "traces", "b.jsonl"), "w") as f:
            f.write(ev(950, "hB", "barrier_start", source="floor", declared=False) + "\n")
            f.write(ev(1000, "hB", "upload", flush="fb1", path="x", outcome="put", etag="e2") + "\n")
            f.write(ev(1200, "hB", "cas", flush="fb1", seq=9, expected="m8", etag="m9", result="ok") + "\n")
            f.write(ev(1250, "hB", "barrier_end", seq=9, uploaded=1, deleted=0, parked=0, consumed=0,
                       no_change=False, ms=300, requests={"get": 3, "head": 1, "put": 1, "copy": 0,
                                                          "delete": 0, "list": 1, "multipart": 0}) + "\n")
            f.write(ev(1900, "hB", "barrier_start", source="floor", declared=False) + "\n")
            f.write(ev(2000, "hB", "cas", flush="fb2", seq=10, expected="m9", result="lost") + "\n")
            f.write(ev(2100, "hB", "barrier_end", seq=9, uploaded=0, deleted=0, parked=0, consumed=0,
                       no_change=True, ms=200, requests=None) + "\n")
        # agent a's journal: its op on x at true 1150 (node n1)
        with open(os.path.join(leg, "agents", "a", "journal.jsonl"), "w") as f:
            f.write(json.dumps({"k": "op", "agent": "a", "n": 1, "t_ms": a_raw(1150), "op": "write", "path": "x",
                                "sha256": "0" * 64, "base": "absent", "nonce": "a-1"}) + "\n")
            f.write(json.dumps({"k": "ack", "agent": "a", "t_ms": a_raw(1600), "nonce": "a-1", "status": "ok",
                                "seq": 9, "covered": ["a-1"]}) + "\n")

        def order(rows):
            return [(r["src"], r["e"].get("ev") or r["e"].get("k")) for r in rows]

        want = [("a", "barrier_start"), ("a", "claim"), ("b", "barrier_start"), ("b", "upload"), ("a", "gc"),
                ("a", "barrier_end"), ("b", "cas"), ("b", "barrier_end"), ("a", "release"),
                ("a", "some_future_event"), ("b", "barrier_start"), ("b", "cas"), ("b", "barrier_end")]
        got = order(collect(leg))
        check("corrected order is the true interleaving", got == want, f"got {got}")
        raw = order(collect(leg, correct=False))
        check("uncorrected order differs (the correction is load-bearing)", raw != want, f"raw {raw}")
        rows = collect(leg)
        a_gc = next(r for r in rows if r["e"].get("ev") == "gc")
        check("interpolated offset applied", abs(a_gc["t"] - (a_gc["raw"] - (200 + 100 * a_gc["raw"] / 2500))) < 1e-6
              and abs(a_gc["t"] - 1200) < 1, f"a.gc at {a_gc['t']}")
        b_up = next(r for r in rows if r["e"].get("ev") == "upload")
        check("constant negative offset applied", b_up["t"] == 1100, f"b.upload at {b_up['t']}")
        got = order(collect(leg, path="x"))
        check("--path keeps only events naming the path", got == [("b", "upload"), ("a", "gc")], f"got {got}")
        got = order(collect(leg, path="x", context=True))
        check("--context adds the enclosing barriers' path-less events, not later barriers",
              got == want[:8], f"got {got}")
        got = order(collect(leg, path="x", journals=True))
        check("--journals merges the op between upload and gc",
              got == [("b", "upload"), ("a", "op"), ("a", "gc")], f"got {got}")
        got = order(collect(leg, t_from=1050, t_to=1260))
        check("--from/--to filter on corrected time",
              got == [("b", "barrier_start"), ("b", "upload"), ("a", "gc"), ("a", "barrier_end")], f"got {got}")
        probs = []
        load_traces(leg, probs)
        check("prose skipped, malformed trace line reported", len(probs) == 1, f"{probs}")
        text = fmt_rows(collect(leg, journals=True))
        check("table renders every row, unknown events included",
              len(text.splitlines()) == 1 + 13 + 2 and "some_future_event" in text and "novel_field" in text, text)
    print(f"timeline selftest: {'PASS' if not fails else 'FAIL ' + ', '.join(fails)}")
    return 0 if not fails else 1


if __name__ == "__main__":
    sys.exit(main())
