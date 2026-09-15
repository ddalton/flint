#!/usr/bin/env python3
"""oracle.py — the writers-live oracles O1–O6 over one collected leg.

    oracle.py <collect/leg> [--idle-from MS --idle-to MS [--request-baseline N]]
              [--faults-declared] [--oracles O1,O2,…] [--wall-slack-ms MS]

Reads the collect layout of README §3 and prints
{"leg", "oracles": {"O1": {"pass", "details"}, …}, "pass"}. Exit 0 iff every
EVALUATED oracle passed, 1 otherwise, 2 on a usage error. An oracle not
evaluated (not selected, or O6 without an idle window) has "pass": null.

The rules, and every decision README §4 left open, are in README §5. The
short form:

O1  checkout/exit.json code == 0; every manifest citation has a HEAD in
    bucket/heads.json whose etag equals the manifest's (quotes and W/ ignored);
    and the checkout digest names exactly the manifest's paths.
O2  every agents/*/tree.sha256 equals checkout/tree.sha256 (extra, missing,
    differing named per agent); every non-"ui" agent in meta.json has a tree.
O3  every acked op part with content h (a write; the `to` half of a mv) is
    accounted for: the checkout's content for the path is h; OR a LATER acked
    op part on the same path has base == h; OR a preserved copy OF THAT PATH
    has sha256 h; OR h was REPLACED: a chain of op parts on the path, acked or
    merely unanswered (never refused, dropped, or any other failure the writer
    was told of), each LATER than and based on the one before (a delete's content is
    `absent`), starts at h and ends at the checkout's content (or a delete,
    the path absent), or at a content a preserved copy of the path holds (a
    peer displaced the rewrite and kept it: the rewrite landed). An agent that
    times out on its ack and rewrites its own file replaced h; a final that
    does not descend from h is still a loss. AND no REFUSED UI write's content
    is the checkout's content or a preserved copy of its path (a refusal means
    "not written"; finding 12's refused PUT landed, and the version it
    destroyed looked replaced once a peer rewrote it). Acked = its nonce is in the `covered` list (or is the nonce)
    of an ack with status ok, or partial without the path in `dropped`.
    Later = ack seq >= when both acks carry an integer seq; otherwise (a UI
    write, a null seq) journal wall time t_ms >= (clock-corrected when the
    leg carries agent_nodes + chrony samples, minus --wall-slack-ms).
    A mv is a delete of `path` (base) plus a write of `to` (to_base, sha256).
O4  bucket/preserved.json keys == the preserved_keys of every
    upload-412-preserved and consume-dirty record in agents/*/conflicts.jsonl
    (and the rotated conflicts.1.jsonl), both ways.
O5  claims across traces/: verdict deadline == 0 and deposals (verdict
    claimed with how deposed) == 0, waiting > 0; how orphaned-own is reported
    only. --faults-declared: deposals are expected, and every deadline must be
    followed, in the same trace, by a `claimed`.
O6  inside [--idle-from, --idle-to]: no cas result ok, no claim claimed, every
    trace has >= 1 barrier_end tick, and with --request-baseline every
    per-tick delta of the summed barrier_end.requests is <= N (and at least
    one delta was measurable per trace). Unknown events and fields are ignored.
"""
import argparse
import glob
import json
import os
import re
import sys
from collections import Counter, defaultdict

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
sys.dont_write_bytecode = True   # no __pycache__ left in the rig directory
import timeline  # noqa: E402  (trace reading + clock correction, shared)

ORACLES = ("O1", "O2", "O3", "O4", "O5", "O6")
DIGEST_LINE = re.compile(r"^([0-9a-fA-F]{64}) [ *](.+)$")
HEX64 = re.compile(r"^[0-9a-f]{64}$")
CONFLICT_KEY = re.compile(r"(?:^|/)\.flint/lean/conflicts/[^/]+/(.+)$")
PRESERVING_KINDS = ("upload-412-preserved", "consume-dirty")
LOSS_CAP = 200


# ---------- evidence loading -------------------------------------------------

def excluded(rel):
    """Not agent content: any component starting .flint (README §3), or a
    temp the syncer's scan skips (name ending .flint-sync-tmp)."""
    parts = rel.split("/")
    return any(p.startswith(".flint") for p in parts) or parts[-1].endswith(".flint-sync-tmp")


def load_json(path, problems):
    try:
        with open(path) as f:
            return json.load(f)
    except FileNotFoundError:
        return None
    except ValueError as e:
        problems.append(f"{path}: not JSON ({e})")
        return None


def read_jsonl(path, problems):
    out = []
    try:
        with open(path, errors="replace") as f:
            for i, line in enumerate(f, 1):
                s = line.strip()
                if not s:
                    continue
                try:
                    out.append((i, json.loads(s)))
                except ValueError:
                    problems.append(f"{path}:{i}: not JSON")
    except FileNotFoundError:
        pass
    return out


def load_digest(path, problems):
    """{path: sha256} or None if the file is absent."""
    if not os.path.exists(path):
        return None
    d = {}
    with open(path, errors="replace") as f:
        for i, line in enumerate(f, 1):
            line = line.rstrip("\n")
            if not line.strip():
                continue
            m = DIGEST_LINE.match(line)
            if not m:
                problems.append(f"{path}:{i}: not a `sha256  path` line")
                continue
            sha, rel = m.group(1).lower(), m.group(2)
            if rel.startswith("./"):
                rel = rel[2:]
            if excluded(rel):
                continue
            if rel in d and d[rel] != sha:
                problems.append(f"{path}:{i}: {rel} listed twice with different digests")
            d[rel] = sha
    return d


def norm_etag(e):
    if e is None:
        return None
    e = str(e).strip()
    if e.startswith("W/"):
        e = e[2:]
    if len(e) >= 2 and e[0] == '"' and e[-1] == '"':
        e = e[1:-1]
    return e


def meta_agents(meta):
    out = []
    for a in meta.get("agents") or []:
        if isinstance(a, dict):
            a = a.get("name") or a.get("id") or a.get("agent")
        if isinstance(a, str):
            out.append(a)
    return out


def is_int(v):
    return isinstance(v, int) and not isinstance(v, bool)


class Leg:
    def __init__(self, root):
        self.root = root
        self.meta = timeline.load_meta(root)
        self.name = self.meta.get("leg") or os.path.basename(os.path.normpath(root))
        self.prefix = (self.meta.get("prefix") or "").strip("/")
        self.clock = timeline.Clock(root)

    def p(self, *rel):
        return os.path.join(self.root, *rel)

    def agent_dirs(self):
        return sorted(d for d in glob.glob(self.p("agents", "*")) if os.path.isdir(d))

    def normkey(self, key):
        key = (key or "").lstrip("/")
        if self.prefix and key.startswith(self.prefix + "/"):
            key = key[len(self.prefix) + 1:]
        return key


# ---------- O1 no dangling ---------------------------------------------------

def oracle_o1(leg):
    problems = []
    ex = load_json(leg.p("checkout", "exit.json"), problems)
    code = ex.get("code") if isinstance(ex, dict) else None
    man = load_json(leg.p("bucket", "manifest.json"), problems)
    entries = man.get("entries") if isinstance(man, dict) else None
    heads = load_json(leg.p("bucket", "heads.json"), problems)
    checkout = load_digest(leg.p("checkout", "tree.sha256"), problems)
    missing_heads, mismatched, extra_heads = [], [], []
    if isinstance(entries, dict) and isinstance(heads, dict):
        for path, ent in sorted(entries.items()):
            if path not in heads:
                missing_heads.append(path)
                continue
            h = heads[path]
            he = h.get("etag") if isinstance(h, dict) else h
            me = ent.get("etag") if isinstance(ent, dict) else None
            if he is None or norm_etag(he) != norm_etag(me):
                mismatched.append({"path": path, "manifest_etag": me, "head_etag": he})
        extra_heads = sorted(set(heads) - set(entries))
    cited = {p for p in (entries or {}) if not excluded(p)}
    digest_missing = sorted(cited - set(checkout)) if checkout is not None else []
    digest_extra = sorted(set(checkout) - cited) if checkout is not None else []
    reasons = []
    if code != 0:
        reasons.append(f"checkout exit code {code!r}")
    if not isinstance(entries, dict):
        reasons.append("bucket/manifest.json has no entries object")
    if not isinstance(heads, dict):
        reasons.append("bucket/heads.json missing or not an object")
    if missing_heads:
        reasons.append(f"{len(missing_heads)} citation(s) with no HEAD")
    if mismatched:
        reasons.append(f"{len(mismatched)} citation(s) whose HEAD etag is not the cited etag")
    if checkout is None:
        reasons.append("checkout/tree.sha256 missing")
    elif digest_missing or digest_extra:
        reasons.append("checkout digest paths != manifest paths")
    o1_pass = not reasons
    return {"pass": o1_pass, "details": {
        "reasons": reasons, "checkout_code": code,
        "stderr_tail": ex.get("stderr_tail") if isinstance(ex, dict) else None,
        "citations": len(entries or {}), "heads": len(heads or {}),
        "missing_heads": missing_heads, "mismatched": mismatched, "extra_heads": extra_heads,
        "checkout_missing_cited": digest_missing, "checkout_not_cited": digest_extra,
        "manifest_seq": man.get("seq") if isinstance(man, dict) else None, "problems": problems}}


# ---------- O2 convergence ---------------------------------------------------

def oracle_o2(leg):
    problems = []
    checkout = load_digest(leg.p("checkout", "tree.sha256"), problems)
    trees = {}
    for d in leg.agent_dirs():
        td = load_digest(os.path.join(d, "tree.sha256"), problems)
        if td is not None:
            trees[os.path.basename(d)] = td
    missing_evidence = [a for a in meta_agents(leg.meta) if a != "ui" and a not in trees]
    diffs = {}
    if checkout is not None:
        for a, td in sorted(trees.items()):
            extra = sorted(set(td) - set(checkout))
            missing = sorted(set(checkout) - set(td))
            differing = sorted(p for p in set(td) & set(checkout) if td[p] != checkout[p])
            if extra or missing or differing:
                diffs[a] = {"extra": extra, "missing": missing,
                            "differing": [{"path": p, "tree": td[p], "checkout": checkout[p]} for p in differing]}
    reasons = []
    if checkout is None:
        reasons.append("checkout/tree.sha256 missing")
    if not trees:
        reasons.append("no agents/*/tree.sha256")
    if missing_evidence:
        reasons.append(f"agents with no tree digest: {missing_evidence}")
    if diffs:
        reasons.append(f"{len(diffs)} tree(s) differ from the checkout")
    o2_pass = checkout is not None and bool(trees) and not missing_evidence and not diffs
    return {"pass": o2_pass, "details": {
        "reasons": reasons, "checkout_files": len(checkout or {}),
        "trees": {a: len(t) for a, t in sorted(trees.items())}, "diffs": diffs, "problems": problems}}


# ---------- O3 nothing acked is lost -----------------------------------------

def build_parts(leg, problems):
    """Every op line, split into parts, each resolved against its ack."""
    covering = defaultdict(dict)       # agent -> nonce -> covering ack line
    own_ack = defaultdict(dict)        # agent -> nonce -> the ack line FOR that nonce
    ops, multiply_covered = [], []
    acks_by_status = Counter()
    for src, lines in sorted(timeline.load_journals(leg.root, problems).items()):
        for o in lines:
            o["_src"] = src
            agent = o.get("agent") or src
            if o.get("k") == "op":
                ops.append(o)
            elif o.get("k") == "ack":
                st = o.get("status")
                acks_by_status[str(st)] += 1
                own_ack[agent][o.get("nonce")] = o
                if st in ("ok", "partial"):
                    cov = [n for n in (o.get("covered") or []) if isinstance(n, str)]
                    if o.get("nonce") not in cov:
                        cov.append(o.get("nonce"))
                    for n in cov:
                        prev = covering[agent].get(n)
                        if prev is None:
                            covering[agent][n] = o
                        elif prev is not o:
                            multiply_covered.append({"agent": agent, "nonce": n,
                                                     "acks": [prev.get("nonce"), o.get("nonce")]})
    parts, malformed = [], []
    for op in ops:
        agent = op.get("agent") or op["_src"]
        nonce = op.get("nonce")
        ack = covering[agent].get(nonce)
        why = None
        if ack is None:
            own = own_ack[agent].get(nonce)
            why = str(own.get("status")) if own else "no-ack-line"
        seq = ack.get("seq") if ack is not None and is_int(ack.get("seq")) else None
        t = op.get("t_ms")
        t = leg.clock.correct(op["_src"], t) if isinstance(t, (int, float)) else None
        dropped = set(ack.get("dropped") or []) if ack is not None and ack.get("status") == "partial" else set()

        def add(kind, path, h, base):
            if not isinstance(path, str) or not path:
                malformed.append({"reason": "op part without a path", "op": clean(op)})
                return
            acked = ack is not None and path not in dropped
            h = h.lower() if isinstance(h, str) else h
            base = base.lower() if isinstance(base, str) else base
            parts.append({"agent": agent, "n": op.get("n"), "kind": kind, "path": path, "h": h, "base": base,
                          "seq": seq, "t": t, "acked": acked,
                          "why": None if acked else ("dropped" if ack is not None else why),
                          "op": op, "ack": ack})

        kind = op.get("op")
        if kind == "write":
            add("write", op.get("path"), op.get("sha256"), op.get("base"))
        elif kind == "delete":
            add("delete", op.get("path"), None, op.get("base"))
        elif kind == "mv":
            add("delete", op.get("path"), None, op.get("base"))
            add("write", op.get("to"), op.get("sha256"), op.get("to_base"))
        else:
            malformed.append({"reason": f"unknown op {kind!r}", "op": clean(op)})
    return parts, malformed, acks_by_status, multiply_covered, len(ops)


def clean(o):
    return {k: v for k, v in (o or {}).items() if not k.startswith("_")} if o is not None else None


def oracle_o3(leg, slack_ms=0):
    problems = []
    parts, malformed, acks_by_status, multiply_covered, n_ops = build_parts(leg, problems)

    final = load_digest(leg.p("checkout", "tree.sha256"), problems)
    final_source = "checkout"
    if final is None:
        final_source = "agent-consensus"
        trees = [load_digest(os.path.join(d, "tree.sha256"), problems) for d in leg.agent_dirs()]
        trees = [t for t in trees if t is not None]
        final = {}
        for p in set().union(*[set(t) for t in trees]) if trees else set():
            vals = {t.get(p) for t in trees}
            if len(vals) == 1 and None not in vals:
                final[p] = vals.pop()

    preserved = load_json(leg.p("bucket", "preserved.json"), problems) or []
    pres_by_path, pres_unparsed = defaultdict(set), set()
    for o in preserved if isinstance(preserved, list) else []:
        sha = str(o.get("sha256") or "").lower()
        m = CONFLICT_KEY.search(str(o.get("key") or ""))
        if m:
            pres_by_path[m.group(1)].add(sha)
        else:
            pres_unparsed.add(sha)

    def later(y, x):
        if y["seq"] is not None and x["seq"] is not None:
            return y["seq"] >= x["seq"]
        return y["t"] is not None and x["t"] is not None and y["t"] + slack_ms >= x["t"]

    by_path = defaultdict(list)
    for p in parts:
        by_path[p["path"]].append(p)

    def may_replace(y):
        # a writer TOLD its op failed (refused, any status but a missing ack)
        # never meant to replace anything: finding 12 was a refused UI write
        # whose PUT had landed anyway. A syncer's partial ack is not that: a
        # DROPPED write is withheld from this boundary and left dirty in the
        # agent's tree for the next barrier to publish (storm S0, 2026-09-15:
        # four writes based on X, dropped, then final or preserved, read as
        # losses of X). It links only where its content is shown to have
        # landed, which `replaced` requires of the chain's end.
        return y["acked"] or y["why"] in ("no-ack", "no-ack-line") or (
            y["why"] == "dropped" and y["agent"] != "ui")

    def replaced(x):
        want = final.get(x["path"])
        kept = pres_by_path.get(x["path"], set()) | pres_unparsed
        seen, frontier = {id(x)}, [x]
        while frontier:
            cur = frontier.pop()
            content = cur["h"] if cur["kind"] == "write" else "absent"
            for y in by_path[x["path"]]:
                if id(y) in seen or not may_replace(y) or y["base"] != content or not later(y, cur):
                    continue
                if (y["kind"] == "write" and (y["h"] == want or y["h"] in kept)) or (
                        y["kind"] == "delete" and want is None):
                    return True
                seen.add(id(y))
                frontier.append(y)
        return False

    accounted = Counter()
    unacked = Counter()
    acked_parts = 0
    acked_writes_by_agent = Counter()
    losses = []
    for x in parts:
        if not x["acked"]:
            unacked[x["why"]] += 1
            continue
        acked_parts += 1
        if x["kind"] != "write":
            continue
        h = x["h"]
        if not isinstance(h, str) or not HEX64.match(h):
            malformed.append({"reason": "acked write without a sha256", "op": clean(x["op"])})
            continue
        acked_writes_by_agent[x["agent"]] += 1
        if final.get(x["path"]) == h:
            accounted["final"] += 1
            continue
        if any(y is not x and y["acked"] and y["base"] == h and later(y, x) for y in by_path[x["path"]]):
            accounted["superseded"] += 1
            continue
        if h in pres_by_path.get(x["path"], ()) or h in pres_unparsed:
            accounted["preserved"] += 1
            continue
        if replaced(x):
            accounted["replaced"] += 1
            continue
        after = [y for y in by_path[x["path"]] if y is not x and (later(y, x) or (
            y["t"] is not None and x["t"] is not None and y["t"] >= x["t"]))]
        losses.append({
            "agent": x["agent"], "n": x["n"], "path": x["path"], "sha256": h,
            "op": clean(x["op"]), "ack": clean(x["ack"]),
            "final": final.get(x["path"], "absent"),
            "unacked_op_with_base_h": any(y["base"] == h and not y["acked"] for y in after),
            "later_ops": [{"agent": y["agent"], "n": y["n"], "part": y["kind"], "op": y["op"].get("op"),
                           "path": y["op"].get("path"), "to": y["op"].get("to"), "base": y["base"],
                           "sha256": y["h"], "t_ms": y["op"].get("t_ms"), "nonce": y["op"].get("nonce"),
                           "acked": y["acked"], "seq": y["seq"], "why_unacked": y["why"],
                           "base_is_h": y["base"] == h} for y in after],
            "timeline": f"timeline.py {leg.root} --path {x['path']} --journals --context"})

    # A gateway refusal promises "not written". A refused UI write whose bytes
    # are the checkout's content or a preserved copy of the path LANDED anyway
    # (finding 12) — and the version it replaced can look REPLACED when a peer
    # had already installed it and rewritten it. Only the UI: a syncer's
    # dropped path may have uploaded before its commit dropped it. A content
    # another op on the path also wrote is ambiguous and skipped.
    refused_landed = []
    for x in parts:
        if x["agent"] != "ui" or x["kind"] != "write" or x["why"] != "refused":
            continue
        h = x["h"]
        if not isinstance(h, str) or any(y is not x and y["h"] == h for y in by_path[x["path"]]):
            continue
        where = "final" if final.get(x["path"]) == h else (
            "preserved" if h in pres_by_path.get(x["path"], ()) or h in pres_unparsed else None)
        if where:
            refused_landed.append({"n": x["n"], "path": x["path"], "sha256": h, "where": where,
                                   "op": clean(x["op"]), "why": x["why"]})

    total_writes = sum(acked_writes_by_agent.values())
    reasons = []
    if losses:
        reasons.append(f"{len(losses)} acked write(s) LOST")
    if refused_landed:
        reasons.append(f"{len(refused_landed)} REFUSED UI write(s) LANDED")
    if malformed:
        reasons.append(f"{len(malformed)} malformed journal op(s)")
    if total_writes == 0:
        reasons.append("no acked writes in any journal (vacuous)")
    o3_pass = not losses and not refused_landed and not malformed and total_writes > 0
    return {"pass": o3_pass, "details": {
        "reasons": reasons, "final_source": final_source, "ops": n_ops, "parts": len(parts),
        "acked_parts": acked_parts, "acked_writes": total_writes,
        "acked_writes_by_agent": dict(sorted(acked_writes_by_agent.items())),
        "accounted": {"final": accounted["final"], "superseded": accounted["superseded"],
                      "replaced": accounted["replaced"],
                      "preserved": accounted["preserved"]},
        "unacked_parts": dict(unacked), "acks_by_status": dict(acks_by_status),
        "preserved_copies": len(preserved) if isinstance(preserved, list) else None,
        "preserved_unparsed_keys": len(pres_unparsed), "multiply_covered": multiply_covered,
        "losses_total": len(losses), "losses": losses[:LOSS_CAP],
        "refused_landed_total": len(refused_landed), "refused_landed": refused_landed[:LOSS_CAP],
        "malformed": malformed[:LOSS_CAP],
        "problems": problems}}


# ---------- O4 records for every preserved object ----------------------------

def oracle_o4(leg):
    problems = []
    preserved = load_json(leg.p("bucket", "preserved.json"), problems)
    objects = Counter(leg.normkey(o.get("key")) for o in preserved) if isinstance(preserved, list) else Counter()
    named = defaultdict(list)
    other_kinds = []
    records = Counter()
    for d in leg.agent_dirs():
        agent = os.path.basename(d)
        for name in ("conflicts.jsonl", "conflicts.1.jsonl"):
            for _, rec in read_jsonl(os.path.join(d, name), problems):
                records[str(rec.get("kind"))] += 1
                key = rec.get("preserved_key")
                if not key:
                    continue
                if rec.get("kind") in PRESERVING_KINDS:
                    named[leg.normkey(key)].append({"agent": agent, "kind": rec.get("kind"), "path": rec.get("path")})
                else:
                    other_kinds.append({"agent": agent, "kind": rec.get("kind"), "key": key})
    no_record = sorted(set(objects) - set(named))
    missing_object = sorted(set(named) - set(objects))
    reasons = []
    if not isinstance(preserved, list):
        reasons.append("bucket/preserved.json missing or not a list")
    if no_record:
        reasons.append(f"{len(no_record)} preserved object(s) with no record")
    if missing_object:
        reasons.append(f"{len(missing_object)} record(s) naming a key that does not exist")
    o4_pass = isinstance(preserved, list) and not no_record and not missing_object
    both_sides = sum(1 for k, v in named.items() if len({r["agent"] for r in v}) >= 2)
    return {"pass": o4_pass, "details": {
        "reasons": reasons, "preserved_objects": sum(objects.values()), "records_by_kind": dict(records),
        "keys_named": len(named), "keys_recorded_by_two_writers": both_sides,
        "duplicate_object_keys": sorted(k for k, c in objects.items() if c > 1),
        "no_record": no_record, "missing_object": missing_object,
        "preserved_key_on_other_kinds": other_kinds, "problems": problems}}


# ---------- O5 fence health --------------------------------------------------

def is_deposal(e):
    """A deposal as the trace is BUILT: a successful claim that took the cell
    from a live-looking holder. There is no `verdict: "deposed"` event."""
    return e.get("ev") == "claim" and e.get("verdict") == "claimed" and e.get("how") == "deposed"


def extract_gaps(root, require):
    """What the trace extractor could NOT turn into events. A spliced line (the
    worker copies the syncer's stderr in raw chunks, and its own messages can
    land mid-line) or an unmapped worker pod hides events — a hidden deadline
    reads as zero deadlines, so any gap fails O5 rather than passing it."""
    p = os.path.join(root, "extract_report.json")
    if not os.path.exists(p):
        return ["no extract_report.json: the traces' completeness is unknown"] if require else []
    try:
        with open(p) as f:
            r = json.load(f)
        m, u, t = int(r["malformed_count"]), len(r["unmapped_pods"]), int(r["unterminated_partials"])
    except (OSError, ValueError, KeyError, TypeError) as e:
        return [f"extract_report.json unreadable: {e}"]
    gaps = []
    if m:
        gaps.append(f"{m} malformed trace line(s): an event may be missing")
    if u:
        gaps.append(f"{u} worker pod(s) mapped to no agent: their traces are not counted")
    if t:
        gaps.append(f"{t} unterminated partial trace record(s)")
    return gaps


def oracle_o5(leg, faults_declared, require_extract_report=False):
    problems = []
    traces = timeline.load_traces(leg.root, problems)
    per, deadlines, deposals, orphaned = {}, [], [], []
    waiting = 0
    for src, evs in sorted(traces.items()):
        c = Counter()
        for idx, e in enumerate(evs):
            if e.get("ev") != "claim":
                continue
            v = e.get("verdict")
            c[f"{v}:{e.get('how')}" if v == "claimed" else str(v)] += 1
            if v == "waiting":
                waiting += 1
            elif v == "deadline":
                recovered = any(f.get("ev") == "claim" and f.get("verdict") == "claimed" for f in evs[idx + 1:])
                deadlines.append({"trace": src, "ts_ms": e.get("ts_ms"), "holder": e.get("holder"),
                                  "behind": e.get("behind"), "waited_ms": e.get("waited_ms"), "recovered": recovered})
            elif is_deposal(e):
                deposals.append({"trace": src, "ts_ms": e.get("ts_ms"), "holder": e.get("holder"),
                                 "epoch": e.get("epoch"), "prior": e.get("prior")})
            elif v == "claimed" and e.get("how") == "orphaned-own":
                orphaned.append({"trace": src, "ts_ms": e.get("ts_ms"), "holder": e.get("holder"),
                                 "epoch": e.get("epoch"), "prior": e.get("prior")})
        per[src] = dict(c)
    reasons = extract_gaps(leg.root, require_extract_report)
    if not traces:
        reasons.append("no traces")
    if faults_declared:
        unrecovered = [d for d in deadlines if not d["recovered"]]
        if unrecovered:
            reasons.append(f"{len(unrecovered)} deadline(s) never followed by a claim in the same trace")
    elif deadlines or deposals:
        reasons.append(f"{len(deadlines)} deadline(s), {len(deposals)} deposal(s) on a no-fault leg")
    if waiting == 0:
        reasons.append("no claim ever waited (no contention: vacuous)")
    o5_pass = not reasons
    return {"pass": o5_pass, "details": {
        "reasons": reasons, "faults_declared": faults_declared, "claims_by_trace": per,
        "waiting": waiting, "deadline": len(deadlines), "deposed": len(deposals),
        "orphaned_own": len(orphaned), "deadlines": deadlines[:LOSS_CAP], "deposals": deposals[:LOSS_CAP],
        "orphaned_own_claims": orphaned[:LOSS_CAP], "problems": problems}}


# ---------- O6 idle quiet ----------------------------------------------------

def oracle_o6(leg, t_from, t_to, baseline):
    problems = []
    traces = timeline.load_traces(leg.root, problems)
    per = {}
    cas_ok, claimed, over, no_deltas = [], [], [], []
    for src, evs in sorted(traces.items()):
        prev_total, ticks, deltas, resets, seqs = None, 0, [], 0, set()
        for e in evs:
            t = leg.clock.correct(src, e["ts_ms"])
            inside = t_from <= t <= t_to
            if inside and e.get("ev") == "cas" and e.get("result") == "ok":
                cas_ok.append({"trace": src, "ts_ms": e["ts_ms"], "seq": e.get("seq")})
            if inside and e.get("ev") == "claim" and e.get("verdict") == "claimed":
                claimed.append({"trace": src, "ts_ms": e["ts_ms"], "epoch": e.get("epoch")})
            if e.get("ev") != "barrier_end":
                continue
            req = e.get("requests")
            total = sum(v for v in req.values() if is_int(v)) if isinstance(req, dict) else None
            if t < t_from:
                prev_total = total
                continue
            if t > t_to:
                continue
            ticks += 1
            seqs.add(e.get("seq"))
            if total is not None and prev_total is not None:
                if total < prev_total:
                    resets += 1
                else:
                    d = total - prev_total
                    deltas.append(d)
                    if baseline is not None and d > baseline:
                        over.append({"trace": src, "ts_ms": e["ts_ms"], "delta": d})
            prev_total = total
        if baseline is not None and ticks and not deltas:
            no_deltas.append(src)
        per[src] = {"ticks": ticks, "max_delta": max(deltas) if deltas else None,
                    "deltas": len(deltas), "counter_resets": resets,
                    "seqs": sorted(s for s in seqs if s is not None)}
    idle_ticks = {s: v["ticks"] for s, v in per.items()}
    no_ticks = sorted(s for s, n in idle_ticks.items() if n == 0)
    reasons = []
    if not traces:
        reasons.append("no traces")
    if cas_ok:
        reasons.append(f"{len(cas_ok)} cas ok inside the idle window")
    if claimed:
        reasons.append(f"{len(claimed)} claim(s) claimed inside the idle window")
    if over:
        reasons.append(f"{len(over)} tick(s) over the request baseline {baseline}")
    if no_ticks:
        reasons.append(f"traces with no barrier_end in the window (vacuous): {no_ticks}")
    if no_deltas:
        reasons.append(f"a request baseline was given but no per-tick delta could be taken "
                       f"(requests null?) (vacuous): {no_deltas}")
    o6_pass = not reasons
    return {"pass": o6_pass, "details": {
        "reasons": reasons, "window": [t_from, t_to], "request_baseline": baseline, "per_trace": per,
        "cas_ok": cas_ok, "claimed": claimed, "over_baseline": over[:LOSS_CAP], "problems": problems}}


# ---------- main ---------------------------------------------------------------

def run(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("leg")
    ap.add_argument("--idle-from", type=float)
    ap.add_argument("--idle-to", type=float)
    ap.add_argument("--request-baseline", type=float)
    ap.add_argument("--faults-declared", action="store_true")
    ap.add_argument("--require-extract-report", action="store_true",
                    help="O5 fails without the extractor's extract_report.json (the deployed verdict)")
    ap.add_argument("--oracles", default=",".join(ORACLES))
    ap.add_argument("--wall-slack-ms", type=float, default=0.0)
    a = ap.parse_args(argv)
    if not os.path.isdir(a.leg):
        print(f"oracle: {a.leg} is not a directory", file=sys.stderr)
        return 2
    if (a.idle_from is None) != (a.idle_to is None):
        print("oracle: --idle-from and --idle-to go together", file=sys.stderr)
        return 2
    selected = [o.strip().upper() for o in a.oracles.split(",") if o.strip()]
    unknown = [o for o in selected if o not in ORACLES]
    if unknown:
        print(f"oracle: unknown oracle(s) {unknown}", file=sys.stderr)
        return 2
    leg = Leg(a.leg)
    results = {}
    for o in ORACLES:
        if o not in selected:
            results[o] = {"pass": None, "skipped": "not selected"}
        elif o == "O1":
            results[o] = oracle_o1(leg)
        elif o == "O2":
            results[o] = oracle_o2(leg)
        elif o == "O3":
            results[o] = oracle_o3(leg, a.wall_slack_ms)
        elif o == "O4":
            results[o] = oracle_o4(leg)
        elif o == "O5":
            results[o] = oracle_o5(leg, a.faults_declared, a.require_extract_report)
        elif o == "O6":
            if a.idle_from is None:
                results[o] = {"pass": None, "skipped": "no --idle-from/--idle-to window"}
            else:
                results[o] = oracle_o6(leg, a.idle_from, a.idle_to, a.request_baseline)
    evaluated = [r["pass"] for r in results.values() if r["pass"] is not None]
    verdict = {"leg": leg.name, "oracles": results, "pass": bool(evaluated) and all(evaluated)}
    print(json.dumps(verdict, indent=2))
    return 0 if verdict["pass"] else 1


if __name__ == "__main__":
    sys.exit(run())
