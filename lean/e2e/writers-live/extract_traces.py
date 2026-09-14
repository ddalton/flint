#!/usr/bin/env python3
"""extract_traces.py — the E1 traces and E3 clock samples of a collected leg,
from the node shipper's evidence.

    extract_traces.py --evidence <pulled>/evidence --out collect/<leg>
                      [--pods FILE ...] [--tenant-ns REGEX] [--agent-map FILE]
                      [--from-ms MS] [--to-ms MS] [--meta collect/<leg>/meta.json]
    extract_traces.py --selftest

Input (README §3 is the output contract; EVIDENCE.md the input layout):
  <evidence>/<node>/pods/<ns>_<pod>_<uid>/<container>/<N>.log[.i<inode>|.<ts>.gz]
      byte copies of the kubelet's CRI logs:
      `<RFC3339Nano> <stdout|stderr> <P|F> <message>`
  <evidence>/<node>/chrony.jsonl          {ts_ms, offset_ms, synced, ...}
  <evidence>/cp/pods.jsonl, pods-watch.jsonl  (podwatch.sh; default --pods)

A worker pod is any pod whose pods.jsonl lines carry the annotation
`chert.us/tenant-pod: <ns>/<pod>`; its agent is --agent-map's value for
"<ns>/<pod>" (or "<pod>"), else the tenant pod's name. Pods are matched by UID
(the evidence directory names it), so a replacement worker with the same name
is a separate pod of the same agent.

Output:
  <out>/traces/<agent>.jsonl   every trace line of every worker pod of that agent:
      CRI prefix stripped, partial (P) records joined with their continuation on
      the same stream (across a rotation boundary too), only messages starting
      `{"ts_ms":` that parse as JSON, in file order. Units (pod, container,
      restart N) are ordered by their first CRI timestamp, and a unit's segments
      (the shipper's rotation copies) by theirs — never by file name.
  <out>/nodes/<node>/chrony.jsonl  the node's synchronised chrony samples
  <out>/agent_nodes.json       {agent: node}; with --meta, merged into meta.json
                               as `agent_nodes` (timeline.py / oracle.py read it)
  <out>/extract_report.json    counts, and everything that was NOT extracted:
      unmapped worker pods (no tenant annotation found for their UID) — their
      trace lines go to <out>/unmapped/<ns>_<pod>_<uid>.jsonl, never to traces/;
      malformed trace lines (start `{"ts_ms":` but are not JSON — e.g. the
      worker's own stderr line spliced into a syncer line by its 4096-byte tee);
      unterminated partial records; duplicate segments; overlapping pods of one
      agent (then O5's "followed by" order across them is not trustworthy).
Which agents: --tenant-ns REGEX on the tenant's namespace; else, when the collect
dir's meta.json lists `agents`, only those (an earlier leg's writers are still in
the evidence); --all-agents disables the filter. A pod directory present under two
node directories (the same UID: a flush under a second NODE name, or podwatch
given the shipper's EVID) is read once, from the larger copy, and reported.
--collect is an alias of --out.
Exit 0; exit 1 if the evidence directory has no node with pod logs.
"""
import argparse
import calendar
import gzip
import json
import os
import re
import shutil
import sys
import tempfile

TRACE_PREFIX = '{"ts_ms":'
TENANT_ANN = "chert.us/tenant-pod"
SEGMENT = re.compile(r"^(\d+)\.log(?:\.i\d+|\.\d{8}-\d{6}(?:\.gz)?)?$")
CRI_TS = re.compile(r"^(\d{4})-(\d\d)-(\d\d)T(\d\d):(\d\d):(\d\d)(?:\.(\d{1,9}))?(Z|[+-]\d\d:\d\d)$")


def cri_ts_ns(s):
    """RFC3339Nano (containerd trims trailing zeros, so never compare as text) -> epoch ns."""
    m = CRI_TS.match(s)
    if not m:
        return None
    y, mo, d, h, mi, se = (int(m.group(i)) for i in range(1, 7))
    secs = calendar.timegm((y, mo, d, h, mi, se, 0, 0, 0))
    if m.group(8) != "Z":
        sign = 1 if m.group(8)[0] == "+" else -1
        secs -= sign * (int(m.group(8)[1:3]) * 3600 + int(m.group(8)[4:6]) * 60)
    return secs * 1_000_000_000 + int((m.group(7) or "0").ljust(9, "0"))


def open_segment(path):
    return gzip.open(path, "rt", encoding="utf-8", errors="replace", newline="") if path.endswith(".gz") \
        else open(path, encoding="utf-8", errors="replace", newline="")


def records(path, report):
    """(ts_ns, stream, tag, message) per CRI record."""
    with open_segment(path) as f:
        for raw in f:
            terminated = raw.endswith("\n")
            line = raw[:-1] if terminated else raw
            if not terminated:
                report["truncated_final_lines"] += 1
            parts = line.split(" ", 3)
            if len(parts) < 3 or parts[1] not in ("stdout", "stderr"):
                report["non_cri_lines"] += 1
                continue
            ts = cri_ts_ns(parts[0])
            if ts is None:
                report["non_cri_lines"] += 1
                continue
            yield ts, parts[1], parts[2], parts[3] if len(parts) == 4 else ""


def first_last_ts(path, report):
    first = last = None
    for ts, _, _, _ in records(path, {"truncated_final_lines": 0, "non_cri_lines": 0}):
        if first is None:
            first = ts
        last = ts
    return first, last


def first_record_bytes(path):
    with open_segment(path) as f:
        return f.readline()


def load_pods(paths, report):
    """uid -> {ns, name, node, tenant}; the tenant from any line of that uid."""
    pods = {}
    for p in paths:
        try:
            f = open(p, errors="replace")
        except FileNotFoundError:
            continue
        with f:
            for line in f:
                try:
                    o = json.loads(line)
                except ValueError:
                    report["pods_lines_unparsed"] += 1
                    continue
                uid = o.get("uid")
                if not uid:
                    continue
                cur = pods.setdefault(uid, {"ns": o.get("ns"), "name": o.get("name"), "node": None, "tenant": None})
                cur["node"] = o.get("node") or cur["node"]
                t = (o.get("annotations") or {}).get(TENANT_ANN)
                if t:
                    if cur["tenant"] and cur["tenant"] != t:
                        report["tenant_conflicts"].append({"uid": uid, "had": cur["tenant"], "now": t})
                    cur["tenant"] = t
    return pods


def agent_name(tenant, agent_map):
    ns, _, pod = tenant.partition("/")
    return agent_map.get(tenant) or agent_map.get(pod) or pod


def dir_bytes(path):
    total = 0
    for d, _, fs in os.walk(path):
        for f in fs:
            try:
                total += os.path.getsize(os.path.join(d, f))
            except OSError:
                pass
    return total


def extract(evidence, out, pods_files=None, tenant_ns=None, agent_map=None, from_ms=None, to_ms=None, meta=None,
            join_partials=True, order_by_time=True, all_agents=False):
    report = {"agents": {}, "unmapped_pods": [], "malformed": [], "malformed_count": 0, "unterminated_partials": 0,
              "truncated_final_lines": 0, "non_cri_lines": 0, "duplicate_segments": [], "overlaps": [],
              "tenant_conflicts": [], "pods_lines_unparsed": 0, "excluded_by_tenant_ns": [], "chrony": {},
              "filtered_by_time": 0, "excluded_not_in_meta_agents": [], "duplicate_pod_dirs": [], "agent_filter": None}
    # Which agents belong to this leg: --tenant-ns, else meta.json's `agents` in the collect dir (so a
    # leg never picks up an earlier leg's writers, whose logs are still in the evidence), else all.
    keep_agents = None
    if not tenant_ns and not all_agents:
        try:
            with open(meta or os.path.join(out, "meta.json")) as f:
                listed = json.load(f).get("agents")
            if isinstance(listed, list) and listed:
                keep_agents = {a for a in listed if isinstance(a, str)}
                report["agent_filter"] = "meta.json agents"
        except (OSError, ValueError):
            pass
    if tenant_ns:
        report["agent_filter"] = f"--tenant-ns {tenant_ns}"
    if report["agent_filter"] is None:
        print("extract_traces: WARNING no --tenant-ns and no meta.json agents: every agent in the evidence is extracted",
              file=sys.stderr)
    agent_map = agent_map or {}
    if pods_files is None:
        pods_files = [os.path.join(evidence, "cp", n) for n in ("pods.jsonl", "pods-watch.jsonl")]
    pods = load_pods(pods_files, report)
    ns_re = re.compile(tenant_ns) if tenant_ns else None

    units = {}  # (dest kind, dest name) -> [unit]
    nodes = sorted(d for d in os.listdir(evidence) if os.path.isdir(os.path.join(evidence, d, "pods")))
    if not nodes:
        print(f"extract_traces: no <node>/pods under {evidence}", file=sys.stderr)
        return None, 1
    agent_nodes = {}
    # The same pod directory (its UID is in the name) under two node dirs is one pod copied twice —
    # a flush under a second NODE name, or podwatch pointed at the shipper's EVID. Keep the larger copy.
    homes = {}
    for node in nodes:
        for pod_dir in os.listdir(os.path.join(evidence, node, "pods")):
            homes.setdefault(pod_dir, []).append(node)
    chosen = {}
    for pod_dir, ns_ in homes.items():
        if len(ns_) == 1:
            chosen[pod_dir] = ns_[0]
            continue
        best = max(sorted(ns_), key=lambda n: (dir_bytes(os.path.join(evidence, n, "pods", pod_dir)), n != "cp"))
        chosen[pod_dir] = best
        report["duplicate_pod_dirs"].append({"pod_dir": pod_dir, "nodes": sorted(ns_), "kept": best})
    for node in nodes:
        for pod_dir in sorted(os.listdir(os.path.join(evidence, node, "pods"))):
            if chosen.get(pod_dir) != node:
                continue
            parts = pod_dir.split("_", 2)
            if len(parts) != 3:
                continue
            ns, pod, uid = parts
            info = pods.get(uid)
            tenant = info and info["tenant"]
            if tenant:
                if ns_re and not ns_re.match(tenant.split("/", 1)[0]):
                    report["excluded_by_tenant_ns"].append(f"{node}/{pod_dir} -> {tenant}")
                    continue
                dest = ("agent", agent_name(tenant, agent_map))
                if keep_agents is not None and dest[1] not in keep_agents:
                    report["excluded_not_in_meta_agents"].append(f"{node}/{pod_dir} -> {dest[1]}")
                    continue
            elif ns == "flint-workers":
                dest = ("unmapped", pod_dir)
            else:
                continue  # an agent, plugin or operator pod: its logs are evidence, not traces
            for container in sorted(os.listdir(os.path.join(evidence, node, "pods", pod_dir))):
                cdir = os.path.join(evidence, node, "pods", pod_dir, container)
                by_n = {}
                for fn in os.listdir(cdir):
                    m = SEGMENT.match(fn)
                    if m:
                        by_n.setdefault(int(m.group(1)), []).append(os.path.join(cdir, fn))
                for n, segs in by_n.items():
                    timed = []
                    for s in segs:
                        first, last = first_last_ts(s, report)
                        if first is not None:
                            timed.append((first, last, s))
                    if not timed:
                        continue
                    timed.sort(key=(lambda t: (t[0], t[2])) if order_by_time else (lambda t: t[2]))
                    kept, seen_first = [], {}
                    for first, last, s in timed:
                        fb = first_record_bytes(s)
                        if fb in seen_first:
                            prev = seen_first[fb]
                            bigger = s if os.path.getsize(s) > os.path.getsize(prev[2]) else prev[2]
                            report["duplicate_segments"].append({"kept": bigger, "dropped": prev[2] if bigger == s else s})
                            if bigger == s:
                                kept[kept.index(prev)] = (first, last, s)
                                seen_first[fb] = (first, last, s)
                            continue
                        seen_first[fb] = (first, last, s)
                        kept.append((first, last, s))
                    units.setdefault(dest, []).append({"node": node, "pod_dir": pod_dir, "container": container, "n": n,
                                                        "first": kept[0][0], "last": max(k[1] for k in kept),
                                                        "segments": [k[2] for k in kept]})
                    if dest[0] == "agent":
                        agent_nodes.setdefault(dest[1], set()).add(node)

    for d in ("traces", "unmapped"):
        shutil.rmtree(os.path.join(out, d), ignore_errors=True)
    os.makedirs(os.path.join(out, "traces"), exist_ok=True)
    for (kind, name), us in sorted(units.items()):
        us.sort(key=(lambda u: (u["first"], u["pod_dir"], u["container"], u["n"])) if order_by_time
                else (lambda u: (u["pod_dir"], u["container"], u["n"])))
        for a, b in zip(us, us[1:]):
            if a["pod_dir"] != b["pod_dir"] and b["first"] < a["last"]:
                report["overlaps"].append({"dest": name, "earlier": a["pod_dir"], "later": b["pod_dir"],
                                           "overlap_ms": (a["last"] - b["first"]) // 1_000_000})
        dest_dir = os.path.join(out, "traces" if kind == "agent" else "unmapped")
        os.makedirs(dest_dir, exist_ok=True)
        n_lines = 0
        with open(os.path.join(dest_dir, f"{name}.jsonl"), "w") as fo:
            for u in us:
                buf = {"stdout": "", "stderr": ""}
                for seg in u["segments"]:
                    for ts, stream, tag, msg in records(seg, report):
                        if tag.split(":")[0] == "P" and join_partials:
                            buf[stream] += msg
                            continue
                        full = buf[stream] + msg
                        buf[stream] = ""
                        if stream != "stderr" or not full.startswith(TRACE_PREFIX):  # the trace is stderr (README §2)
                            continue
                        try:
                            ev = json.loads(full)
                        except ValueError:
                            report["malformed_count"] += 1
                            if len(report["malformed"]) < 20:
                                report["malformed"].append({"segment": seg, "line": full[:300]})
                            continue
                        t = ev.get("ts_ms")
                        if (from_ms is not None and isinstance(t, (int, float)) and t < from_ms) or \
                                (to_ms is not None and isinstance(t, (int, float)) and t > to_ms):
                            report["filtered_by_time"] += 1
                            continue
                        fo.write(full + "\n")
                        n_lines += 1
                report["unterminated_partials"] += sum(1 for v in buf.values() if v)
        entry = {"lines": n_lines, "units": [f"{u['node']}/{u['pod_dir']}/{u['container']}/{u['n']}" for u in us],
                 "segments": sum(len(u["segments"]) for u in us)}
        if kind == "agent":
            entry["nodes"] = sorted(agent_nodes.get(name, ()))
            report["agents"][name] = entry
        else:
            report["unmapped_pods"].append(dict(entry, pod_dir=name))

    for node in sorted(os.listdir(evidence)):
        src = os.path.join(evidence, node, "chrony.jsonl")
        if not os.path.isfile(src):
            continue
        kept = dropped = 0
        os.makedirs(os.path.join(out, "nodes", node), exist_ok=True)
        with open(src, errors="replace") as fi, open(os.path.join(out, "nodes", node, "chrony.jsonl"), "w") as fo:
            for line in fi:
                try:
                    o = json.loads(line)
                    ok = isinstance(o.get("ts_ms"), (int, float)) and isinstance(o.get("offset_ms"), (int, float)) \
                        and o.get("synced", True) is not False
                except ValueError:
                    ok = False
                if ok:
                    fo.write(json.dumps(o, sort_keys=True) + "\n")
                    kept += 1
                else:
                    dropped += 1
        report["chrony"][node] = {"kept": kept, "dropped_unsynced_or_malformed": dropped}

    an = {a: sorted(ns)[0] for a, ns in agent_nodes.items() if len(ns) == 1}
    multi = {a: sorted(ns) for a, ns in agent_nodes.items() if len(ns) > 1}
    if multi:
        report["agents_on_several_nodes"] = multi
    with open(os.path.join(out, "agent_nodes.json"), "w") as f:
        json.dump(an, f, indent=1, sort_keys=True)
    if meta:
        try:
            with open(meta) as f:
                m = json.load(f)
        except (OSError, ValueError):
            m = {}
        m.setdefault("agent_nodes", {}).update(an)
        with open(meta, "w") as f:
            json.dump(m, f, indent=1, sort_keys=True)
    with open(os.path.join(out, "extract_report.json"), "w") as f:
        json.dump(report, f, indent=1, sort_keys=True)
    return report, 0


# ----------------------------------------------------------------- selftest --

def cri(ts, stream, tag, msg):
    return f"{ts} {stream} {tag} {msg}\n"


def T(sec, frac=""):
    return f"2026-09-13T10:00:{sec:02d}{('.' + frac) if frac else ''}Z"


def tl(ts_ms, ev, **kw):
    return json.dumps(dict({"ts_ms": ts_ms, "mono_ms": 0, "holder": "h", "ev": ev}, **kw), separators=(",", ":"))


def build_fixture(base):
    ev = os.path.join(base, "evidence")
    w1 = os.path.join(ev, "n1", "pods", "flint-workers_s3w-aaaa_uid-w1", "worker")
    w1b = os.path.join(ev, "n1", "pods", "flint-workers_s3w-aaaa_uid-w1b", "worker")
    w3 = os.path.join(ev, "n1", "pods", "flint-workers_s3w-cccc_uid-w3", "worker")
    ag = os.path.join(ev, "n1", "pods", "wl-a1_agents-x_uid-a1", "agent")
    w2 = os.path.join(ev, "n2", "pods", "flint-workers_s3w-bbbb_uid-w2", "worker")
    wx = os.path.join(ev, "n2", "pods", "flint-workers_s3w-dddd_uid-wx", "worker")
    for d in (w1, w1b, w3, ag, w2, wx, os.path.join(ev, "cp")):
        os.makedirs(d, exist_ok=True)
    L = {}
    # agent a1's first worker: the OLDER rotation segment is named 0.log.i222 (sorts AFTER 0.log)
    L["a1"] = [tl(1000, "barrier_start"), tl(1001, "scan", flush="f1"), tl(1002, "merge", flush="f1", upserts=2),
               tl(1003, "cas", flush="f1", result="ok"), tl(1004, "gc", flush="f1", path="x"),
               tl(1005, "ack", status="ok"), tl(1006, "barrier_start"), tl(1007, "claim", verdict="claimed")]
    big = L["a1"][2]
    with open(os.path.join(w1, "0.log.i222"), "w") as f:
        f.write(cri(T(0, "5"), "stderr", "F", "flint-s3-worker: launching /usr/local/bin/flint-sync --x"))
        f.write(cri(T(0, "50001"), "stderr", "F", L["a1"][0]))
        # a P/F pair on stderr with a stdout record between them
        f.write(cri(T(0, "6"), "stderr", "P", L["a1"][1][:10]))
        f.write(cri(T(0, "61"), "stdout", "F", '{"ts_ms":999999,"ev":"stdout-noise-but-json"}'))
        f.write(cri(T(0, "62"), "stderr", "F", L["a1"][1][10:]))
        f.write(cri(T(1), "stderr", "F", "flint-sync: prose line mentioning {\"ts_ms\": but not at the start"))
        # a record split in three, whose LAST piece is in the next rotation segment
        f.write(cri(T(1, "5"), "stderr", "P", big[:7]))
        f.write(cri(T(1, "500000001"), "stderr", "P", big[7:20]))
    with open(os.path.join(w1, "0.log"), "w") as f:
        f.write(cri(T(2), "stderr", "F", big[20:]))
        f.write(cri(T(2, "1"), "stderr", "F", L["a1"][3]))
        f.write(cri(T(2, "2"), "stderr", "F", '{"ts_ms":1003,"mono_flint-s3-worker: forwarding signal 15 to pid 7'))
        f.write(cri(T(2, "3"), "stderr", "F", 'ms":1,"holder":"h","ev":"cas"}'))
        f.write(cri(T(2, "4"), "stderr", "F", L["a1"][4]))
    with open(os.path.join(w1, "1.log"), "w") as f:  # container restart, later
        f.write(cri(T(10), "stderr", "F", L["a1"][5]))
    with open(os.path.join(w1b, "0.log"), "w") as f:  # the replacement worker pod (same name, new uid)
        f.write(cri(T(20), "stderr", "F", L["a1"][6]))
        f.write(cri(T(21), "stderr", "F", L["a1"][7]))
    with open(os.path.join(w3, "0.log"), "w") as f:  # no pods.jsonl entry at all
        f.write(cri(T(5), "stderr", "F", tl(5000, "claim", verdict="waiting")))
    with open(os.path.join(ag, "0.log"), "w") as f:  # the AGENT's own log: never a trace
        f.write(cri(T(3), "stdout", "F", tl(3000, "agent-echo")))
    # agent a2 on n2: an older .gz segment and the live 0.log
    L["a2"] = [tl(2000, "barrier_start"), tl(2001, "upload", flush="g", path="p", outcome="put"), tl(2002, "barrier_end")]
    with gzip.open(os.path.join(w2, "0.log.20260913-100000.gz"), "wt") as f:
        f.write(cri("2026-09-13T09:59:59.9Z", "stderr", "F", L["a2"][0]))
        f.write(cri("2026-09-13T10:00:00Z", "stderr", "F", L["a2"][1]))
    with open(os.path.join(w2, "0.log"), "w") as f:
        f.write(cri("2026-09-13T10:00:00.000000001Z", "stderr", "F", L["a2"][2]))
    # a byte-identical duplicate of the .gz content left uncompressed (a restarted shipper's re-copy)
    with gzip.open(os.path.join(w2, "0.log.20260913-100000.gz"), "rt") as fi, \
            open(os.path.join(w2, "0.log.i999"), "w") as fo:
        fo.write(fi.read()[: len(cri("2026-09-13T09:59:59.9Z", "stderr", "F", L["a2"][0]))])
    with open(os.path.join(wx, "0.log"), "w") as f:  # a tenant outside --tenant-ns
        f.write(cri(T(4), "stderr", "F", tl(4000, "claim")))
    snap = [
        {"src": "snap", "uid": "uid-w1", "ns": "flint-workers", "name": "s3w-aaaa", "node": "n1",
         "annotations": {TENANT_ANN: "wl-a1/agents-x"}},
        {"src": "snap", "uid": "uid-a1", "ns": "wl-a1", "name": "agents-x", "node": "n1", "annotations": {}},
        {"src": "snap", "uid": "uid-w2", "ns": "flint-workers", "name": "s3w-bbbb", "node": "n2",
         "annotations": {TENANT_ANN: "wl-a1/agents-y"}},
        {"src": "snap", "uid": "uid-wx", "ns": "flint-workers", "name": "s3w-dddd", "node": "n2",
         "annotations": {TENANT_ANN: "wl-other/agents-z"}},
        {"src": "snap", "uid": "uid-we", "ns": "flint-workers", "name": "s3w-eeee", "node": "n2",
         "annotations": {TENANT_ANN: "wl-a1/agents-old"}},
    ]
    with open(os.path.join(ev, "cp", "pods.jsonl"), "w") as f:
        for o in snap:
            f.write(json.dumps(o) + "\n")
        f.write("not json\n")
    with open(os.path.join(ev, "cp", "pods-watch.jsonl"), "w") as f:  # the short-lived replacement: only the watch saw it
        f.write(json.dumps({"src": "watch", "uid": "uid-w1b", "ns": "flint-workers", "name": "s3w-aaaa", "node": "n1",
                            "annotations": {TENANT_ANN: "wl-a1/agents-x"}}) + "\n")
    # the same worker pod dir flushed a second time under another NODE name, older (smaller)
    dup = os.path.join(ev, "ip-10-0-0-1", "pods", "flint-workers_s3w-aaaa_uid-w1b", "worker")
    os.makedirs(dup, exist_ok=True)
    with open(os.path.join(dup, "0.log"), "w") as f:
        f.write(cri(T(20), "stderr", "F", L["a1"][6]))
    # an EARLIER leg's writer, same tenant namespace, still in the evidence; not in meta.json agents
    old_leg = os.path.join(ev, "n2", "pods", "flint-workers_s3w-eeee_uid-we", "worker")
    os.makedirs(old_leg, exist_ok=True)
    with open(os.path.join(old_leg, "0.log"), "w") as f:
        f.write(cri(T(1), "stderr", "F", tl(100, "claim", verdict="claimed", how="fresh")))
    with open(os.path.join(ev, "n1", "chrony.jsonl"), "w") as f:
        f.write(json.dumps({"ts_ms": 900, "offset_ms": 12.5, "synced": True, "leap": "Normal"}) + "\n")
        f.write(json.dumps({"ts_ms": 960, "offset_ms": 0.0, "synced": False, "leap": "Not synchronised"}) + "\n")
        f.write("{broken\n")
    with open(os.path.join(base, "agent-map.json"), "w") as f:
        json.dump({"wl-a1/agents-y": "a2"}, f)
    os.makedirs(os.path.join(base, "collect"), exist_ok=True)
    with open(os.path.join(base, "collect", "meta.json"), "w") as f:
        json.dump({"leg": "A1", "agents": ["agents-x", "a2"]}, f)
    return ev, L


def run_checks(base, **mut):
    ev, L = build_fixture(base)
    out = os.path.join(base, "collect")
    with open(os.path.join(base, "agent-map.json")) as f:
        amap = json.load(f)
    # as mac.sh calls it: no --tenant-ns, so meta.json's `agents` decide which writers belong to the leg
    report, rc = extract(ev, out, agent_map=amap, meta=os.path.join(out, "meta.json"), **mut)
    results = []

    def check(name, ok, detail=""):
        results.append((name, bool(ok), detail))

    def read(p):
        try:
            with open(p) as f:
                return f.read().splitlines()
        except FileNotFoundError:
            return None

    check("exit 0", rc == 0, rc)
    got = read(os.path.join(out, "traces", "agents-x.jsonl"))
    check("agent a1 (tenant pod name, no map): exact lines, in order — rotation segments by first CRI timestamp, "
          "a P record joined across stdout noise and across the rotation boundary, the container restart and the "
          "replacement pod after", got == L["a1"], {"got": got, "want": L["a1"]})
    got2 = read(os.path.join(out, "traces", "a2.jsonl"))
    check("agent a2 (--agent-map): .gz segment first, the uncompressed duplicate dropped",
          got2 == L["a2"], {"got": got2, "want": L["a2"]})
    check("traces/ holds exactly the two agents (the agent pod's own log, the excluded tenant and the unmapped worker are not there)",
          sorted(os.listdir(os.path.join(out, "traces"))) == ["a2.jsonl", "agents-x.jsonl"], os.listdir(os.path.join(out, "traces")))
    check("a stdout record between a stderr P and its F is neither joined in nor emitted (the trace is stderr)",
          got is not None and not any("stdout-noise" in l for l in got), got)
    check("unmapped worker pod reported and written to unmapped/, not traces/",
          report and [u["pod_dir"] for u in report["unmapped_pods"]] == ["flint-workers_s3w-cccc_uid-w3"]
          and read(os.path.join(out, "unmapped", "flint-workers_s3w-cccc_uid-w3.jsonl")) is not None)
    check("the spliced (worker tee) line counted malformed, its tail not emitted", report and report["malformed_count"] == 1
          and "forwarding signal" in report["malformed"][0]["line"], report and report["malformed"])
    check("no unterminated partial left", report and report["unterminated_partials"] == 0, report and report["unterminated_partials"])
    check("duplicate segment reported", report and len(report["duplicate_segments"]) == 1, report and report["duplicate_segments"])
    check("writers not in meta.json agents (another namespace, an EARLIER leg's writer) excluded and reported",
          report and sorted(x.split(" -> ")[1] for x in report["excluded_not_in_meta_agents"]) == ["agents-old", "agents-z"],
          report and report["excluded_not_in_meta_agents"])
    check("a pod dir copied under a second node name is read once, from the larger copy, and reported",
          report and [(d["pod_dir"], d["kept"]) for d in report["duplicate_pod_dirs"]] == [("flint-workers_s3w-aaaa_uid-w1b", "n1")],
          report and report["duplicate_pod_dirs"])
    ch = read(os.path.join(out, "nodes", "n1", "chrony.jsonl"))
    check("chrony: the synchronised sample kept, unsynced and malformed dropped",
          ch is not None and len(ch) == 1 and json.loads(ch[0])["offset_ms"] == 12.5
          and report["chrony"]["n1"]["dropped_unsynced_or_malformed"] == 2, ch)
    with open(os.path.join(out, "meta.json")) as f:
        m = json.load(f)
    check("agent_nodes merged into meta.json", m.get("agent_nodes") == {"agents-x": "n1", "a2": "n2"} and m.get("leg") == "A1", m)
    check("no overlap between the two worker pods of agent a1", report and report["overlaps"] == [], report and report["overlaps"])
    return results, out


def selftest():
    fails = 0
    with tempfile.TemporaryDirectory() as base:
        results, out = run_checks(base)
        print("extract_traces on a synthetic evidence tree")
        for name, ok, detail in results:
            print(f"  {'PASS' if ok else 'FAIL'}  {name}" + ("" if ok else f"\n        {detail}"))
            fails += not ok
        # the output through timeline.py, as the drill reads it
        import subprocess
        r = subprocess.run([sys.executable, os.path.join(os.path.dirname(os.path.abspath(__file__)), "timeline.py"),
                            out, "--json"], capture_output=True, text=True)
        try:
            rows = json.loads(r.stdout)
        except ValueError:
            rows = None
        ok = r.returncode == 0 and rows is not None and len(rows) == 11 and \
            any(x["src"] == "agents-x" and x["skew_ms"] == 12.5 for x in rows)
        print(f"  {'PASS' if ok else 'FAIL'}  timeline.py reads the output: 11 events, agents-x corrected by n1's chrony offset"
              + ("" if ok else f"\n        rc={r.returncode} {r.stderr[-300:]} rows={rows and len(rows)}"))
        fails += not ok
    with tempfile.TemporaryDirectory() as base:
        import subprocess
        ev, L = build_fixture(base)
        me = os.path.abspath(__file__)
        r = subprocess.run([sys.executable, me, "--evidence", ev, "--collect", os.path.join(base, "collect"),
                            "--agent-map", os.path.join(base, "agent-map.json"), "--tenant-ns", "^wl-a1$"],
                           capture_output=True, text=True)
        names = sorted(os.listdir(os.path.join(base, "collect", "traces"))) if r.returncode == 0 else None
        ok = names == ["a2.jsonl", "agents-old.jsonl", "agents-x.jsonl"]
        print(f"  {'PASS' if ok else 'FAIL'}  CLI --collect with --tenant-ns ^wl-a1$: the namespace filter alone also takes "
              f"the earlier leg's writer (why meta.json agents is the default filter)" + ("" if ok else f"\n        {r.returncode} {names} {r.stderr[-300:]}"))
        fails += not ok
        r = subprocess.run([sys.executable, me, "--evidence", ev, "--collect", os.path.join(base, "collect"),
                            "--agent-map", os.path.join(base, "agent-map.json"), "--all-agents"], capture_output=True, text=True)
        names = sorted(os.listdir(os.path.join(base, "collect", "traces"))) if r.returncode == 0 else None
        ok = names == ["a2.jsonl", "agents-old.jsonl", "agents-x.jsonl", "agents-z.jsonl"]
        print(f"  {'PASS' if ok else 'FAIL'}  CLI --all-agents: every mapped writer" + ("" if ok else f"\n        {names}"))
        fails += not ok
    for label, mut in (("P records NOT joined", {"join_partials": False}),
                       ("segments ordered by NAME, not first timestamp", {"order_by_time": False})):
        with tempfile.TemporaryDirectory() as base:
            results, _ = run_checks(base, **mut)
            caught = [n for n, ok, _ in results if not ok]
            print(f"  {'PASS' if caught else 'FAIL'}  control — {label}: the checks fail ({len(caught)} failed)")
            fails += not caught
    print(f"\nextract_traces selftest: {'OK' if not fails else f'{fails} FAILED'}")
    return 1 if fails else 0


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--evidence")
    ap.add_argument("--out", "--collect", dest="out")
    ap.add_argument("--all-agents", action="store_true", help="no agent filter even when meta.json lists agents")
    ap.add_argument("--pods", action="append")
    ap.add_argument("--tenant-ns")
    ap.add_argument("--agent-map")
    ap.add_argument("--from-ms", type=float)
    ap.add_argument("--to-ms", type=float)
    ap.add_argument("--meta")
    ap.add_argument("--selftest", action="store_true")
    a = ap.parse_args(argv)
    if a.selftest:
        return selftest()
    if not a.evidence or not a.out:
        ap.error("--evidence and --out (or --collect) are required")
    amap = {}
    if a.agent_map:
        with open(a.agent_map) as f:
            amap = json.load(f)
    report, rc = extract(a.evidence, a.out, a.pods, a.tenant_ns, amap, a.from_ms, a.to_ms, a.meta,
                         all_agents=a.all_agents)
    if report is not None:
        summary = {k: report[k] for k in ("agents", "malformed_count", "unterminated_partials", "overlaps",
                                           "duplicate_segments", "chrony")}
        summary["unmapped_pods"] = [u["pod_dir"] for u in report["unmapped_pods"]]
        print(json.dumps(summary, indent=1, sort_keys=True))
        if report["unmapped_pods"] or report["malformed_count"] or report["overlaps"]:
            print("extract_traces: WARNING — unmapped pods, malformed lines or overlapping pods; "
                  "see extract_report.json", file=sys.stderr)
    return rc


if __name__ == "__main__":
    sys.exit(main())
