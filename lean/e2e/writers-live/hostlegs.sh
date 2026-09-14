#!/usr/bin/env bash
# hostlegs.sh — the HOST LEGS of the writers live drill
# (docs/plans/flint-lean-writers-live-drill-plan.md §3.2, §4).
#
# Each leg opens ONE two-writer race on purpose, with two `flint-sync`
# processes on one host (a held window does not need two machines), and
# judges it with oracles that can fail. Each has a CONTROL arm: the same
# script on a binary with exactly that one fix disabled
# (make_control.py + control-patches/). The fixed arm must PASS; the
# control arm must FAIL with the defect's signature; a control that
# passes makes the leg VOID — never green.
#
#   hostlegs.sh H1 fixed|control   F1 the GC gap          (FLINT_SYNC_DRILL_HOLD_GC_SECS)
#   hostlegs.sh H2 fixed|control   F2 the adopt window    (FLINT_SYNC_DRILL_HOLD_COMMIT_SECS)
#   hostlegs.sh H3 fixed|control   F3 the sync overlay    (gateway HITL write + GC hold + `sync`)
#   hostlegs.sh H5 fixed           finding 10 (OPEN): a writer killed between its upload and
#                                  its CAS. MEASURES the residual; expected to FAIL on the fixed
#                                  binary, and that failure is the measurement.
#   hostlegs.sh probe              `flint-sync probe-conditional` -> $OUT/probe.txt
#   hostlegs.sh selftest           the verdict code against synthetic legs (no store)
#   hostlegs.sh fakes3 start|stop  LOCAL ONLY: the repo's fake S3 (see "fakes3" below)
#
# Environment:
#   FSYNC_FIXED          the fixed flint-sync (every arm's ORACLE reads use it too: the
#                        fresh checkout into C and `flint-sync manifest`, so the oracle
#                        never moves with the arm)
#   FSYNC_CONTROL_H1     control binary for H1 (make_control.py f1-unconditional-gc)
#   FSYNC_CONTROL_H2     control binary for H2 (f2-no-commit-reread)
#   FSYNC_CONTROL_H3     control binary for H3 (f3-sync-advances-hidden-base)
#   GATEWAY_BIN          flint-lean-gateway, for H3's HITL write
#   BUCKET               (required)
#   ENDPOINT             empty = real S3 (IMDS or env credentials); else e.g.
#                        http://127.0.0.1:9000 (path-style; dummy credentials if none set)
#   AWS_REGION           default us-west-1
#   ROOT                 writer roots, default /mnt/nvme/h
#   PREFIX_BASE          default h — each leg runs on its own fresh h/<leg>-<arm>-<ts>
#   OUT                  results, default $ROOT/out; a leg writes $OUT/<leg>-<arm>/
#   HOLD_GC_SECS         default 20   HOLD_COMMIT_SECS default 30
#   WAIT_SECS            bound on every wait (default 180). Exceeding it VOIDs the leg;
#                        it never orders anything — ordering is read from the trace.
#   QUIESCE_ROUNDS       barrier rounds per live writer after the race (default 3)
#   H5_MODE              commit-hold (default: kill -9 inside A's held commit section,
#                        after its uploads, before its CAS; B then deposes the dead
#                        holder, ~60 s) | mid-upload (A uploads p1 then a big p2 with
#                        FLINT_SYNC_UPLOAD_FANOUT=1; killed once p1's key moves and before
#                        p2 lands or any claim — the unit test's exact shape)
#   H5_BIG_MB            mid-upload's p2 size (default 512)
#   SKIP_PROBE=1         skip the per-leg store probe (the verdict records it as skipped)
#
# Every leg first runs `probe-conditional` on a sibling prefix. A store that does not
# enforce If-Match/If-None-Match on PUT and If-Match on DELETE cannot host these races:
# the leg is VOID before any writer starts. A double missing a property passes or fails
# for the wrong reason.
#
# Output per leg ($OUT/<leg>-<arm>/): A.log B.log C.log (stderr of every process that
# writer ran, trace lines included, with `### <ts_ms> step=<label> start|exit rc=N`
# marks), G.log (gateway), *.stdout, facts.jsonl (fixture checks), manifest.*.json,
# digest.<who>.txt, meta.json, verdict.json. The last line printed is
#   LEG <leg> <arm> PASS|FAIL|VOID <reason>
# Exit: 0 = the arm met its expectation (fixed PASS, control FAIL with the signature,
# H5 FAIL with the finding-10 signature); 3 = VOID; 1 = anything else.
#
# fakes3 (lean/e2e/perf/fakes3): as of 2026-09-13 it does NOT model conditional PUT
# (If-Match / If-None-Match are ignored), CopyObject, user metadata or the CRC-64 on HEAD
# — the pointer CAS, the lease cell, the 412 adopt and the preserve path all depend on
# them — so the probe gate VOIDs every leg there. Kept for exercising the rig's plumbing.
set -u -o pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
CMD=${1:-}
ARM=${2:-}

ROOT=${ROOT:-/mnt/nvme/h}
PREFIX_BASE=${PREFIX_BASE:-h}
OUT=${OUT:-$ROOT/out}
ENDPOINT=${ENDPOINT:-}
AWS_REGION=${AWS_REGION:-us-west-1}
HOLD_GC_SECS=${HOLD_GC_SECS:-20}
HOLD_COMMIT_SECS=${HOLD_COMMIT_SECS:-30}
WAIT_SECS=${WAIT_SECS:-180}
QUIESCE_ROUNDS=${QUIESCE_ROUNDS:-3}
H5_MODE=${H5_MODE:-commit-hold}
H5_BIG_MB=${H5_BIG_MB:-512}
GW_PORT=${GW_PORT:-18091}
GW_TOKEN=${GW_TOKEN:-hostlegs-drill-token-0123456789}

die() { echo "hostlegs: $*" >&2; exit 2; }
now_ms() { python3 -c 'import time; print(int(time.time()*1000))'; }
sha() { if command -v sha256sum >/dev/null; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi; }

# ── the python half: waits, digests, verdicts, self-test ─────────────────
PYLIB=$(mktemp "${TMPDIR:-/tmp}/hostlegs-lib.XXXXXX")
trap 'rm -f "$PYLIB"' EXIT
cat > "$PYLIB" <<'PY'
import hashlib, json, os, sys, time

def norm(e):
    if e is None:
        return None
    e = str(e).strip()
    if e.startswith("W/"):
        e = e[2:]
    return e.strip('"')

def lines(path):
    try:
        with open(path, encoding="utf-8", errors="replace") as f:
            return f.read().splitlines()
    except FileNotFoundError:
        return []

def parse(path):
    """[(kind, obj)] in file order: ('ev', trace dict) | ('mark', {ts, step, what, rc})."""
    out = []
    for l in lines(path):
        if l.startswith('{"ts_ms":'):
            try:
                out.append(("ev", json.loads(l)))
            except ValueError:
                pass
        elif l.startswith("### "):
            parts = l.split(" ", 3)
            if len(parts) >= 4 and parts[2].startswith("step="):
                what = parts[3]
                m = {"ts": int(parts[1]), "step": parts[2][5:], "what": what.split(" ")[0]}
                if "rc=" in what:
                    try:
                        m["rc"] = int(what.split("rc=")[1].split()[0])
                    except ValueError:
                        pass
                out.append(("mark", m))
        else:
            out.append(("prose", l))
    return out

def segment(path, step):
    """Events (and prose) between `step=<step> start` and its exit (or EOF)."""
    evs, prose, on, mark = [], [], False, {}
    for kind, o in parse(path):
        if kind == "mark" and o["step"] == step:
            if o["what"] == "start":
                on, evs, prose, mark = True, [], [], {"start": o["ts"]}
            elif on:
                mark.update({"end": o["ts"], "rc": o.get("rc")})
                on = False
                break
        elif on and kind == "ev":
            evs.append(o)
        elif on and kind == "prose":
            prose.append(o)
    return evs, prose, mark

def match(e, kv):
    for k, v in kv.items():
        if str(e.get(k)).lower() != str(v).lower():
            return False
    return True

def first(evs, **kv):
    for e in evs:
        if match(e, kv):
            return e
    return None

def alive(pid):
    if pid in ("-", "", None):
        return True
    try:
        os.kill(int(pid), 0)
    except OSError:
        return False
    # a zombie (exited, not yet reaped by the shell) is not alive
    try:
        import subprocess
        st = subprocess.run(["ps", "-o", "stat=", "-p", str(pid)], capture_output=True, text=True).stdout.strip()
        return bool(st) and not st.startswith("Z")
    except OSError:
        return True

def cmd_wait(args):
    # wait <log> <step> <pid|-> <timeout> (ev k=v ... | prose <substring>)
    log, step, pid, timeout = args[0], args[1], args[2], float(args[3])
    rest = args[4:]
    deadline = time.time() + timeout
    while True:
        evs, prose, _ = segment(log, step)
        if rest[0] == "prose":
            if any(rest[1] in p for p in prose):
                return 0
        else:
            kv = dict(a.split("=", 1) for a in rest)
            if first(evs, **kv):
                return 0
        if not alive(pid):
            # one last look: the line may have landed just before the exit
            evs, prose, _ = segment(log, step)
            hit = any(rest[1] in p for p in prose) if rest[0] == "prose" else first(evs, **dict(a.split("=", 1) for a in rest))
            return 0 if hit else 2
        if time.time() > deadline:
            return 1
        time.sleep(0.05)

EXCL = {".flint", ".flint-sync"}

def cmd_digest(args):
    root = args[0]
    rows = []
    for d, dirs, files in os.walk(root):
        dirs[:] = [x for x in dirs if x not in EXCL and not x.endswith(".flint-sync-tmp")]
        for f in files:
            if f.endswith(".flint-sync-tmp") or f in EXCL:
                continue
            p = os.path.join(d, f)
            if os.path.islink(p) or not os.path.isfile(p):
                continue
            h = hashlib.sha256(open(p, "rb").read()).hexdigest()
            rows.append(f"{h}  {os.path.relpath(p, root)}")
    print("\n".join(sorted(rows, key=lambda r: r.split("  ", 1)[1])))
    return 0

def cmd_json(args):
    # json <file> <python expr over `d`>  — tiny accessor for the shell
    try:
        d = json.load(open(args[0]))
        v = eval(args[1], {"d": d, "norm": norm})
    except Exception as e:  # an unreadable manifest reads as empty; the caller's fact fails
        print(f"hostlegs: json {args[0]}: {e}", file=sys.stderr)
        print("")
        return 1
    print("" if v is None else (v if isinstance(v, str) else json.dumps(v)))
    return 0

# ── verdicts ─────────────────────────────────────────────────────────────
def load_json(p):
    try:
        return json.load(open(p))
    except (FileNotFoundError, ValueError):
        return None

def load_digest(p):
    if not os.path.isfile(p):
        return None
    out = {}
    for l in lines(p):
        if "  " in l:
            h, path = l.split("  ", 1)
            out[path] = h
    return out

def o1(m, rc):
    probs = []
    if m is None:
        return False, ["no manifest read"]
    if rc is not None and rc != 0:
        probs.append(f"fresh checkout exit {rc}")
    if m.get("seq") is None:
        probs.append("no manifest at the prefix")
    for p, e in sorted((m.get("entries") or {}).items()):
        heads = m.get("heads") or {}
        if p not in heads:
            probs.append(f"{p}: no HEAD recorded")
        elif heads[p] is None:
            probs.append(f"{p}: cited {norm(e.get('etag'))} but HEAD 404")
        elif norm(heads[p]) != norm(e.get("etag")):
            probs.append(f"{p}: cited {norm(e.get('etag'))} but HEAD {norm(heads[p])}")
    return not probs, probs

def o2(ld, live):
    c = load_digest(os.path.join(ld, "digest.C.txt"))
    if c is None:
        return False, ["no digest of the fresh checkout C"], {}
    probs, diffs = [], {}
    for w in live:
        t = load_digest(os.path.join(ld, f"digest.{w}.txt"))
        if t is None:
            probs.append(f"no digest of live writer {w}")
            continue
        extra = sorted(set(t) - set(c))
        missing = sorted(set(c) - set(t))
        differ = sorted(p for p in set(t) & set(c) if t[p] != c[p])
        diffs[w] = {"extra_in_writer": extra, "missing_in_writer": missing, "differ": differ}
        for p in extra:
            probs.append(f"{w} has {p}, C does not")
        for p in missing:
            probs.append(f"C has {p}, {w} does not")
        for p in differ:
            probs.append(f"{p} differs between {w} and C")
    return not probs, probs, diffs

def facts(ld):
    out = []
    for l in lines(os.path.join(ld, "facts.jsonl")):
        try:
            out.append(json.loads(l))
        except ValueError:
            pass
    return out

def ts(e):
    return e["ts_ms"] if e else None

def guard_h1(ld, arm, m_race):
    a, _, _ = segment(os.path.join(ld, "A.log"), "race-a")
    b, _, bmark = segment(os.path.join(ld, "B.log"), "race-b")
    hold = first(a, ev="drill_hold", where="gc", path="x.txt")
    put = first(b, ev="upload", path="x.txt", outcome="put")
    gc = None
    if hold:
        gc = next((e for e in a if e.get("ev") == "gc" and e.get("path") == "x.txt" and e["ts_ms"] >= hold["ts_ms"]), None)
    g = []
    if not hold: g.append("A never held its GC on x.txt")
    if not put: g.append("B's upload of x.txt never landed as a PUT")
    if not gc: g.append("A's GC of x.txt never ran")
    if hold and bmark.get("start") is not None and bmark["start"] < hold["ts_ms"]:
        g.append("B was started before A's hold (the script's order broke)")
    if hold and put and gc and not (hold["ts_ms"] <= put["ts_ms"] <= gc["ts_ms"]):
        g.append(f"B's PUT ({put['ts_ms']}) is not inside A's hold ({hold['ts_ms']}..{gc['ts_ms']})")
    ev = {"hold": hold, "put": put, "gc": gc}
    if g:
        return False, g, ev, None, None
    # The fixed arm's own proof of the interleaving (no clock): A's re-HEAD saw B's etag.
    fixed_ok = gc.get("result") == "skip" and norm(gc.get("head")) == norm(put.get("etag"))
    fixed_why = f"A's GC result={gc.get('result')} head={norm(gc.get('head'))} (want skip at B's {norm(put.get('etag'))})"
    ent = (m_race or {}).get("entries", {}).get("x.txt")
    head = (m_race or {}).get("heads", {}).get("x.txt", "absent")
    sig = (gc.get("result") == "deleted" and ent is not None and norm(ent.get("etag")) == norm(put.get("etag")) and head is None)
    sig_why = (f"F1 signature: A's GC deleted x.txt unconditionally (head {norm(gc.get('head'))}) after B's PUT "
               f"{norm(put.get('etag'))} landed; B's commit cites it and HEAD is 404")
    return True, [], ev, (fixed_ok, fixed_why), (sig, sig_why)

def guard_h2(ld, arm, m_race):
    a, _, amark = segment(os.path.join(ld, "A.log"), "race-a")
    b, bprose, _ = segment(os.path.join(ld, "B.log"), "race-b")
    held = any("DRILL: holding the fence" in p for p in bprose)
    bclaim = first(b, ev="claim", verdict="claimed")
    bgc = first(b, ev="gc", path="x.txt")
    adopt = first(a, ev="upload", path="x.txt", outcome="adopted")
    aclaim = first(a, ev="claim", verdict="claimed")
    await_ = first(a, ev="claim", verdict="waiting")
    g = []
    if not held: g.append("B never held its commit section")
    if not bclaim: g.append("B never claimed")
    if not bgc: g.append("B's GC never reached x.txt")
    elif bgc.get("result") != "deleted": g.append(f"B's GC of x.txt was {bgc.get('result')}, not deleted")
    if not adopt: g.append("A never adopted x.txt (no 412 -> adopt: the window did not open)")
    if not aclaim: g.append("A's commit never claimed")
    if bclaim and amark.get("start") is not None and amark["start"] < bclaim["ts_ms"]:
        g.append("A was started before B held the fence (the script's order broke)")
    if adopt and bgc and aclaim and not (adopt["ts_ms"] <= bgc["ts_ms"] <= aclaim["ts_ms"]):
        g.append(f"not adopt({adopt['ts_ms']}) <= B's GC delete({bgc['ts_ms']}) <= A's claim({aclaim['ts_ms']})")
    if adopt and not await_:
        g.append("A's claim never waited behind B")
    ev = {"b_claim": bclaim, "b_gc": bgc, "adopt": adopt, "a_claim": aclaim}
    if g:
        return False, g, ev, None, None
    obs = first(a, ev="observed", path="x.txt")
    fixed_ok = obs is not None and obs.get("still") is False
    fixed_why = f"A's commit-section re-read of x.txt: {obs}"
    ent = (m_race or {}).get("entries", {}).get("x.txt")
    head = (m_race or {}).get("heads", {}).get("x.txt", "absent")
    sig = (obs is None and ent is not None and norm(ent.get("etag")) == norm(adopt.get("etag")) and head is None)
    sig_why = (f"F2 signature: A adopted x.txt at {norm(adopt.get('etag'))}, B's GC deleted it before A's claim, "
               f"no re-read ran, and A's commit cites it with HEAD 404")
    return True, [], ev, (fixed_ok, fixed_why), (sig, sig_why)

def guard_h3(ld, arm, m_race):
    a, _, _ = segment(os.path.join(ld, "A.log"), "race-a")
    b, _, bmark = segment(os.path.join(ld, "B.log"), "race-sync")
    hold = first(a, ev="drill_hold", where="gc", path="x.txt")
    gc = None
    if hold:
        gc = next((e for e in a if e.get("ev") == "gc" and e.get("path") == "x.txt" and e["ts_ms"] >= hold["ts_ms"]), None)
    sync = first(b, ev="sync")
    fx = {f["name"]: f for f in facts(ld)}
    g = []
    if not hold: g.append("A never held its GC on x.txt")
    if not gc: g.append("A's GC of x.txt never ran")
    elif gc.get("result") != "deleted": g.append(f"A's GC of x.txt was {gc.get('result')}, not deleted")
    if not sync: g.append("B's sync emitted no trace")
    elif not (isinstance(sync.get("hidden"), int) and sync["hidden"] >= 1):
        g.append(f"B's sync saw no overlay-hidden path (hidden={sync.get('hidden')})")
    if hold and bmark.get("start") is not None and bmark["start"] < hold["ts_ms"]:
        g.append("B's sync was started before A's hold (before A's CAS)")
    if hold and sync and gc and not (hold["ts_ms"] <= sync["ts_ms"] <= gc["ts_ms"]):
        g.append(f"B's sync ({sync['ts_ms']}) is not inside A's hold ({hold['ts_ms']}..{gc['ts_ms']})")
    if not fx.get("b-applied-hitl", {}).get("ok"):
        g.append("B's sync did not apply the HITL bytes (fact b-applied-hitl)")
    ev = {"hold": hold, "sync": sync, "gc": gc}
    if g:
        return False, g, ev, None, None
    dB = load_digest(os.path.join(ld, "digest.B.txt")) or {}
    dC = load_digest(os.path.join(ld, "digest.C.txt")) or {}
    fixed_ok = "x.txt" not in dB
    fixed_why = "B's tree has no x.txt" if fixed_ok else "B's tree still has x.txt"
    sig = "x.txt" in dB and "x.txt" not in dC
    sig_why = "F3 signature: B's sync advanced its base past A's delete; B keeps x.txt, the manifest and a fresh checkout do not"
    return True, [], ev, (fixed_ok, fixed_why), (sig, sig_why)

def guard_h5(ld, arm, m_race):
    a, aprose, amark = segment(os.path.join(ld, "A.log"), "race-a")
    fx = {f["name"]: f for f in facts(ld)}
    mode = (load_json(os.path.join(ld, "meta.json")) or {}).get("h5_mode", "commit-hold")
    kill = load_json(os.path.join(ld, "manifest.at_kill.json"))
    pre = load_json(os.path.join(ld, "manifest.pre.json"))
    g = []
    if not fx.get("a-killed", {}).get("ok"):
        g.append("A was not killed by the script (fact a-killed)")
    if first(a, ev="cas"):
        g.append("A reached a CAS before it was killed")
    p1_moved = False
    if kill and pre:
        c1 = norm((pre.get("entries") or {}).get("p1.txt", {}).get("etag"))
        h1 = norm((kill.get("heads") or {}).get("p1.txt"))
        p1_moved = c1 is not None and h1 is not None and h1 != c1
    if not p1_moved:
        g.append("p1.txt's key did not hold A's upload at the kill")
    if mode == "commit-hold":
        if not any("DRILL: holding the fence" in p for p in aprose):
            g.append("A never reached its held commit section")
        if not first(a, ev="upload", path="p1.txt", outcome="put"):
            g.append("A's trace has no PUT of p1.txt")
    else:
        if first(a, ev="claim"):
            g.append("A claimed before the kill (mid-upload wants a kill before any claim)")
        if kill and pre:
            c2 = norm((pre.get("entries") or {}).get("p2.bin", {}).get("etag"))
            h2 = norm((kill.get("heads") or {}).get("p2.bin"))
            if h2 != c2:
                g.append("p2.bin had already landed at the kill")
    if g:
        return False, g, {}, None, None
    dB = load_digest(os.path.join(ld, "digest.B.txt")) or {}
    dC = load_digest(os.path.join(ld, "digest.C.txt")) or {}
    fin = load_json(os.path.join(ld, "manifest.final.json")) or {}
    div = sorted(p for p in set(dB) | set(dC) if dB.get(p) != dC.get(p))
    moved = sorted(p for p, e in (fin.get("entries") or {}).items()
                   if norm((fin.get("heads") or {}).get(p)) != norm(e.get("etag")))
    sig = bool(div) or bool(moved)
    sig_why = (f"finding-10 residual: B and a fresh checkout disagree on {div}; "
               f"citations whose key holds other bytes: {moved}")
    return True, [], {}, (not sig, "converged"), (sig, sig_why)

GUARDS = {"H1": guard_h1, "H2": guard_h2, "H3": guard_h3, "H5": guard_h5}
LIVE = {"H1": ["A", "B"], "H2": ["A", "B"], "H3": ["A", "B"], "H5": ["B"]}
DEFECT = {"H1": "F1", "H2": "F2", "H3": "F3", "H5": "finding 10"}

def verdict(leg, arm, ld):
    v = {"leg": leg, "arm": arm, "dir": ld}
    fx = facts(ld)
    probe = open(os.path.join(ld, "probe.txt")).read().strip() if os.path.isfile(os.path.join(ld, "probe.txt")) else None
    v["probe"] = probe
    v["facts"] = fx
    expected = "FAIL" if (arm == "control" or leg == "H5") else "PASS"
    v["expected"] = expected

    def done(status, reason, meets):
        v.update({"status": status, "reason": reason, "meets_expectation": meets})
        return v

    if probe is None:
        return done("VOID", "no store probe recorded", False)
    if "SKIPPED" not in probe and "PASS" not in probe:
        return done("VOID", f"the store failed probe-conditional, it cannot host these races: {probe}", False)
    bad = [f for f in fx if not f.get("ok") and f.get("fixture", True)]
    if bad:
        return done("VOID", "setup did not reach the race: " + "; ".join(f"{f['name']}: {f.get('detail')}" for f in bad), False)

    m_race = load_json(os.path.join(ld, "manifest.race.json"))
    m_final = load_json(os.path.join(ld, "manifest.final.json"))
    rc_c = None
    try:
        rc_c = int(open(os.path.join(ld, "C.checkout.rc")).read().strip())
    except (FileNotFoundError, ValueError):
        rc_c = -1
    ok_g, why_g, evs, fixed_chk, sig_chk = GUARDS[leg](ld, arm, m_race)
    v["race_events"] = evs
    if not ok_g:
        return done("VOID", "the race was not opened: " + "; ".join(why_g), False)

    r1_ok, r1 = o1(m_race, None) if leg != "H5" else (True, ["not judged: H5's race state is the kill"])
    f1_ok, f1 = o1(m_final, rc_c)
    o2_ok, o2p, diffs = o2(ld, LIVE[leg])
    post = [f for f in fx if f.get("name", "").startswith("post-rc") and not f.get("ok")]
    v["oracles"] = {
        "O1_race": {"pass": r1_ok, "problems": r1},
        "O1_final": {"pass": f1_ok, "problems": f1},
        "O2": {"pass": o2_ok, "problems": o2p, "diffs": diffs},
    }
    v["post_barrier_failures"] = [f["detail"] for f in post]
    all_ok = r1_ok and f1_ok and o2_ok
    probs = [f"O1(race) {p}" for p in r1 if not r1_ok] + [f"O1 {p}" for p in f1 if not f1_ok] + [f"O2 {p}" for p in o2p]

    if leg == "H5":
        sig, why = sig_chk
        if sig:
            return done("FAIL", f"(expected: {why}) oracles: " + "; ".join(probs[:6]), True)
        return done("PASS", "UNEXPECTED: finding 10 did not reproduce — B and a fresh checkout agree; re-read the guard before believing a fix", False)
    if arm == "fixed":
        fok, fwhy = fixed_chk
        if all_ok and fok and not post:
            return done("PASS", f"O1 O2 clean; {fwhy}", True)
        extra = [] if fok else [f"fixed-arm check failed: {fwhy}"]
        extra += [f"a writer barrier after the race failed: {d}" for d in v["post_barrier_failures"]]
        return done("FAIL", "; ".join(extra + probs[:8]), False)
    # control
    if all_ok:
        return done("VOID", f"the control PASSED every oracle: the {DEFECT[leg]} race did not bite here, so the leg proves nothing", False)
    sig, why = sig_chk
    if sig:
        return done("FAIL", f"(expected: {why}) oracles: " + "; ".join(probs[:6]), True)
    return done("FAIL", f"control failed WITHOUT the {DEFECT[leg]} signature ({why} not seen): " + "; ".join(probs[:8]), False)

def cmd_verdict(args):
    leg, arm, ld = args
    v = verdict(leg, arm, ld)
    with open(os.path.join(ld, "verdict.json"), "w") as f:
        json.dump(v, f, indent=2, default=str)
    print(f"LEG {leg} {arm} {v['status']} {v['reason']}")
    if v["status"] == "VOID":
        return 3
    return 0 if v["meets_expectation"] else 1

# ── self-test: synthetic legs, each fault must be classified ─────────────
def cmd_selftest(args):
    import tempfile
    T0 = 1757800000000
    def tr(holder, ev, t, **f):
        d = {"ts_ms": T0 + t, "mono_ms": t, "holder": holder, "ev": ev}
        d.update(f)
        return json.dumps(d)
    def mk(leg, arm, files):
        ld = tempfile.mkdtemp(prefix=f"hl-{leg}-{arm}-")
        for name, body in files.items():
            with open(os.path.join(ld, name), "w") as f:
                f.write(body if isinstance(body, str) else json.dumps(body))
        return ld
    def mark(step, what, t, rc=None):
        return f"### {T0 + t} step={step} {what}" + (f" rc={rc}" if rc is not None else "")
    def man(entries, heads, seq=3):
        return {"seq": seq, "entries": {p: {"etag": e} for p, e in entries.items()}, "heads": heads}
    same = "aa  x.txt\nbb  keep.txt\n"
    base = {"probe.txt": "flint-sync: probe-conditional PASS — PUT (k) and DELETE (k2)", "C.checkout.rc": "0",
            "digest.A.txt": same, "digest.B.txt": same, "digest.C.txt": same, "facts.jsonl": ""}
    def h1(put_t=2000, gc_res="skip", gc_head='"EB"', race_heads=None, final_heads=None, rc="0", digB=same):
        A = "\n".join([mark("race-a", "start", 0), tr("a", "drill_hold", 500, where="gc", path="x.txt", secs=20),
                       tr("a", "gc", 20500, path="x.txt", head=gc_head, result=gc_res), mark("race-a", "exit", 20600, 0)])
        B = "\n".join([mark("race-b", "start", 600), tr("b", "upload", put_t, path="x.txt", outcome="put", etag='"EB"'),
                       mark("race-b", "exit", 21000, 0)])
        f = dict(base, **{"A.log": A, "B.log": B, "C.checkout.rc": rc, "digest.B.txt": digB})
        f["manifest.race.json"] = man({"x.txt": '"EB"'}, race_heads if race_heads is not None else {"x.txt": '"EB"'})
        f["manifest.final.json"] = man({"x.txt": '"EB"'}, final_heads if final_heads is not None else {"x.txt": '"EB"'})
        return f
    def h2(adopt=True, obs=True, heads_null=False, aclaim_t=30400):
        B = "\n".join([mark("race-b", "start", 0), "flint-sync: DRILL: holding the fence for 30s inside the commit section",
                       tr("b", "claim", 100, verdict="claimed", how="fresh", epoch=3),
                       tr("b", "gc", 30200, path="x.txt", head='"S"', result="deleted"), mark("race-b", "exit", 30300, 0)])
        al = [mark("race-a", "start", 300)]
        if adopt:
            al.append(tr("a", "upload", 900, path="x.txt", outcome="adopted", etag='"S"'))
        al.append(tr("a", "claim", 11000, verdict="waiting", behind="b", quiet_polls=0, waited_ms=1000))
        al.append(tr("a", "claim", aclaim_t, verdict="claimed", how="released", epoch=4))
        if obs:
            al.append(tr("a", "observed", 30410, path="x.txt", etag='"S"', still=False))
        al.append(mark("race-a", "exit", 30500, 0))
        f = dict(base, **{"A.log": "\n".join(al), "B.log": B})
        if heads_null:
            f["manifest.race.json"] = man({"x.txt": '"S"'}, {"x.txt": None})
            f["manifest.final.json"] = man({"x.txt": '"S"'}, {"x.txt": None})
            f["C.checkout.rc"] = "1"
        else:
            f["manifest.race.json"] = man({}, {})
            f["manifest.final.json"] = man({"x.txt": '"A2"'}, {"x.txt": '"A2"'})
        return f
    def h3(hidden=1, sync_t=5000, keepB=False, applied=True):
        A = "\n".join([mark("race-a", "start", 0), tr("a", "drill_hold", 400, where="gc", path="x.txt", secs=20),
                       tr("a", "gc", 20400, path="x.txt", head='"H"', result="deleted"), mark("race-a", "exit", 20500, 0)])
        B = "\n".join([mark("race-sync", "start", 450), tr("b", "sync", sync_t, scoped=False, applied=1, deleted=0, conflicts=0, seq=3, hidden=hidden),
                       mark("race-sync", "exit", sync_t + 10, 0)])
        d = "bb  keep.txt\n"
        f = dict(base, **{"A.log": A, "B.log": B, "digest.A.txt": d, "digest.C.txt": d,
                          "digest.B.txt": ("cc  x.txt\n" + d) if keepB else d,
                          "facts.jsonl": json.dumps({"name": "b-applied-hitl", "ok": applied, "detail": "x"}) + "\n"})
        f["manifest.race.json"] = man({"keep.txt": '"K"'}, {"keep.txt": '"K"'})
        f["manifest.final.json"] = man({"keep.txt": '"K"'}, {"keep.txt": '"K"'})
        return f
    def h5(cas=False, moved=True, diverged=True):
        al = [mark("race-a", "start", 0), tr("a", "upload", 1000, path="p1.txt", outcome="put", etag='"A1"'),
              tr("a", "claim", 1100, verdict="claimed", how="fresh", epoch=2),
              "flint-sync: DRILL: holding the fence for 30s inside the commit section"]
        if cas:
            al.append(tr("a", "cas", 1200, seq=2, expected="p", result="ok", etag="q"))
        al.append(mark("race-a", "exit", 1300, 137))
        f = dict(base, **{"A.log": "\n".join(al), "B.log": "",
                          "facts.jsonl": json.dumps({"name": "a-killed", "ok": True, "detail": "137"}) + "\n",
                          "meta.json": {"h5_mode": "commit-hold"},
                          "digest.B.txt": "s1  p1.txt\n", "digest.C.txt": ("a1  p1.txt\n" if diverged else "s1  p1.txt\n")})
        f["manifest.pre.json"] = man({"p1.txt": '"S1"', "p2.bin": '"S2"'}, {"p1.txt": '"S1"', "p2.bin": '"S2"'})
        f["manifest.at_kill.json"] = man({"p1.txt": '"S1"', "p2.bin": '"S2"'}, {"p1.txt": '"A1"' if moved else '"S1"', "p2.bin": '"A2"'})
        f["manifest.final.json"] = man({"p1.txt": '"S1"'}, {"p1.txt": '"A1"' if diverged else '"S1"'})
        return f
    cases = [
        ("H1 fixed clean", "H1", "fixed", h1(), "PASS", True),
        ("H1 control, F1 bites", "H1", "control", h1(gc_res="deleted", gc_head='"SEED"', race_heads={"x.txt": None}, final_heads={"x.txt": None}, rc="1"), "FAIL", True),
        ("H1 fixed, dangling anyway", "H1", "fixed", h1(gc_res="deleted", gc_head='"SEED"', race_heads={"x.txt": None}, final_heads={"x.txt": None}, rc="1"), "FAIL", False),
        ("H1 control passes -> VOID", "H1", "control", h1(), "VOID", False),
        ("H1 fixed, B's PUT after A's GC -> VOID (never a vacuous PASS)", "H1", "fixed", h1(put_t=21000), "VOID", False),
        ("H1 control, B's PUT after A's GC -> VOID", "H1", "control", h1(put_t=21000), "VOID", False),
        ("H1 control fails w/o signature", "H1", "control", h1(digB="zz  x.txt\nbb  keep.txt\n"), "FAIL", False),
        ("H1 probe failed -> VOID", "H1", "fixed", dict(h1(), **{"probe.txt": "flint-sync: probe-conditional FAIL (PUT): x"}), "VOID", False),
        ("H1 fixed, a post-race barrier failed -> FAIL", "H1", "fixed", dict(h1(), **{"facts.jsonl": json.dumps({"name": "post-rc-A-1", "ok": False, "detail": "A barrier round 1 exit 1", "fixture": False}) + "\n"}), "FAIL", False),
        ("H1 fixture failed -> VOID", "H1", "fixed", dict(h1(), **{"facts.jsonl": json.dumps({"name": "b-has-seed", "ok": False, "detail": "no"}) + "\n"}), "VOID", False),
        ("H2 fixed clean", "H2", "fixed", h2(), "PASS", True),
        ("H2 control, F2 bites", "H2", "control", h2(obs=False, heads_null=True), "FAIL", True),
        ("H2 no adopt -> VOID", "H2", "control", h2(adopt=False, obs=False, heads_null=True), "VOID", False),
        ("H2 fixed, A committed before B's GC -> VOID (never a vacuous PASS)", "H2", "fixed", h2(aclaim_t=20000), "VOID", False),
        ("H3 fixed clean", "H3", "fixed", h3(), "PASS", True),
        ("H3 control, F3 bites", "H3", "control", h3(keepB=True), "FAIL", True),
        ("H3 nothing hidden -> VOID", "H3", "control", h3(hidden=0, keepB=True), "VOID", False),
        ("H3 sync after the GC -> VOID", "H3", "control", h3(sync_t=21000, keepB=True), "VOID", False),
        ("H3 fixed, sync after the GC -> VOID (never a vacuous PASS)", "H3", "fixed", h3(sync_t=21000), "VOID", False),
        ("H3 fixed, B kept x", "H3", "fixed", h3(keepB=True), "FAIL", False),
        ("H5 residual measured", "H5", "fixed", h5(), "FAIL", True),
        ("H5 converged -> unexpected PASS", "H5", "fixed", h5(diverged=False), "PASS", False),
        ("H5 CAS before kill -> VOID", "H5", "fixed", h5(cas=True), "VOID", False),
        ("H5 p1 not moved -> VOID", "H5", "fixed", h5(moved=False), "VOID", False),
    ]
    failed = 0
    for name, leg, arm, files, want, meets in cases:
        ld = mk(leg, arm, files)
        v = verdict(leg, arm, ld)
        ok = v["status"] == want and bool(v["meets_expectation"]) == meets
        failed += 0 if ok else 1
        print(f"{'ok  ' if ok else 'BAD '} {name}: {v['status']} meets={v['meets_expectation']} — {v['reason'][:150]}")
    print(f"selftest: {len(cases) - failed}/{len(cases)}")
    return 1 if failed else 0

CMDS = {"wait": cmd_wait, "digest": cmd_digest, "json": cmd_json, "verdict": cmd_verdict, "selftest": cmd_selftest}
sys.exit(CMDS[sys.argv[1]](sys.argv[2:]))
PY
py() { python3 "$PYLIB" "$@"; }

# ── the rig ───────────────────────────────────────────────────────────────
STORE_ENV=()
store_env() {
  [ -n "${BUCKET:-}" ] || die "BUCKET is required"
  STORE_ENV=(AWS_REGION="$AWS_REGION")
  if [ -n "$ENDPOINT" ]; then
    STORE_ENV+=(FLINT_SYNC_ENDPOINT="$ENDPOINT" FLINT_LEAN_GW_ENDPOINT="$ENDPOINT"
                AWS_ACCESS_KEY_ID="${AWS_ACCESS_KEY_ID:-hostlegs}"
                AWS_SECRET_ACCESS_KEY="${AWS_SECRET_ACCESS_KEY:-hostlegs}"
                AWS_EC2_METADATA_DISABLED=true)
  fi
}

# alive <pid>: running, and not a zombie waiting to be reaped (kill -0 succeeds on those)
alive() { local s; s=$(ps -o stat= -p "$1" 2>/dev/null) || return 1; case "$s" in Z*|"") return 1 ;; esac; return 0; }

mark() { printf '### %s step=%s %s\n' "$(now_ms)" "$2" "$3" >> "$LD/$1.log"; }

fact() { # <name> <ok:true|false> <detail> [fixture:true|false]
  python3 -c 'import json,sys; print(json.dumps({"name":sys.argv[1],"ok":sys.argv[2]=="true","detail":sys.argv[3],"fixture":sys.argv[4]=="true"},separators=(",",":")))' \
    "$1" "$2" "$3" "${4:-true}" >> "$LD/facts.jsonl"
  [ "$2" = true ] || echo "  fact FAILED: $1 — $3"
}

# fs <who> <step> <bin> <root> <verb> [VAR=VAL...] — foreground; returns the exit code.
fs() {
  local who=$1 step=$2 bin=$3 root=$4 verb=$5; shift 5
  mark "$who" "$step" start
  env -u FLINT_SYNC_ENDPOINT -u FLINT_LEAN_GW_ENDPOINT "${STORE_ENV[@]}" \
    FLINT_SYNC_BUCKET="$BUCKET" FLINT_SYNC_PREFIX="$PFX" FLINT_SYNC_ROOT="$root" \
    FLINT_SYNC_EVENT_TRACE=1 "$@" "$bin" "$verb" >> "$LD/$who.stdout" 2>> "$LD/$who.log"
  local rc=$?
  mark "$who" "$step" "exit rc=$rc"
  return $rc
}

# fs_bg: the same, in the background; the pid is left in BG_PID. The exit
# mark is written by bg_wait, after the process is reaped.
fs_bg() {
  local who=$1 step=$2 bin=$3 root=$4 verb=$5; shift 5
  mark "$who" "$step" start
  env -u FLINT_SYNC_ENDPOINT -u FLINT_LEAN_GW_ENDPOINT "${STORE_ENV[@]}" \
    FLINT_SYNC_BUCKET="$BUCKET" FLINT_SYNC_PREFIX="$PFX" FLINT_SYNC_ROOT="$root" \
    FLINT_SYNC_EVENT_TRACE=1 "$@" "$bin" "$verb" >> "$LD/$who.stdout" 2>> "$LD/$who.log" &
  BG_PID=$!
}

# bg_wait <who> <step> <pid> <timeout> — reaps the process, writes its exit
# mark, returns its exit code (124 after killing it at the bound).
bg_wait() {
  local who=$1 step=$2 pid=$3 t=$4 i=0 rc
  while alive "$pid"; do
    if [ "$i" -ge $((t * 5)) ]; then
      kill -9 "$pid" 2>/dev/null; wait "$pid" 2>/dev/null
      mark "$who" "$step" "exit rc=124"
      return 124
    fi
    i=$((i + 1)); sleep 0.2
  done
  wait "$pid"; rc=$?
  mark "$who" "$step" "exit rc=$rc"
  return $rc
}

# await <who> <step> <pid|-> <timeout> <ev k=v...| prose substring>
await() { py wait "$LD/$1.log" "$2" "$3" "$4" "${@:5}"; }

# manifest <file> — the oracle's read of the bucket (fixed binary, no lease, no tree)
manifest() {
  env -u FLINT_SYNC_ENDPOINT -u FLINT_LEAN_GW_ENDPOINT "${STORE_ENV[@]}" \
    FLINT_SYNC_BUCKET="$BUCKET" FLINT_SYNC_PREFIX="$PFX" FLINT_SYNC_ROOT="$RUN/O" \
    "$FSYNC_FIXED" manifest > "$1" 2>> "$LD/O.log"
}
jget() { py json "$1" "$2"; }

writefile() { mkdir -p "$(dirname "$1")"; printf '%s' "$2" > "$1"; }
readfile() { [ -f "$1" ] && cat "$1" || printf '<absent>'; }

probe_store() { # <prefix> <outfile> <scratch root: the probe opens a state dir there>
  local line
  mkdir -p "$3"
  env -u FLINT_SYNC_ENDPOINT -u FLINT_LEAN_GW_ENDPOINT "${STORE_ENV[@]}" \
    FLINT_SYNC_BUCKET="$BUCKET" FLINT_SYNC_PREFIX="$1" FLINT_SYNC_ROOT="$3" \
    "$FSYNC_FIXED" probe-conditional > "$2.stdout" 2> "$2.stderr"
  local rc=$?
  line=$(grep -E 'probe-conditional (PASS|FAIL)' "$2.stderr" | tail -1)
  [ -n "$line" ] || line="flint-sync: probe-conditional FAIL (no verdict line; exit $rc): $(tail -2 "$2.stderr" | tr '\n' ' ')"
  printf '%s\n' "$line" > "$2"
  echo "  probe: $line"
  return $rc
}

setup_leg() { # <leg> <arm>
  LEG=$1
  store_env
  [ -n "${FSYNC_FIXED:-}" ] && [ -x "$FSYNC_FIXED" ] || die "FSYNC_FIXED must name an executable flint-sync"
  case "$ARM" in
    fixed) BIN=$FSYNC_FIXED ;;
    control)
      [ "$LEG" != H5 ] || die "H5 has no control arm: it measures an OPEN residual on the fixed binary"
      local var="FSYNC_CONTROL_$LEG"
      BIN=${!var:-}
      [ -n "$BIN" ] && [ -x "$BIN" ] || die "$var must name the control flint-sync for $LEG"
      ;;
    *) die "arm must be fixed|control" ;;
  esac
  TS=$(date -u +%Y%m%dT%H%M%SZ)
  PFX="$PREFIX_BASE/$(echo "$LEG" | tr 'A-Z' 'a-z')-$ARM-$TS"
  RUN="$ROOT/$LEG-$ARM-$TS"
  LD="$OUT/$LEG-$ARM"
  if [ -e "$LD" ]; then mv "$LD" "$LD.before-$TS"; fi
  mkdir -p "$LD" "$RUN/A" "$RUN/B" "$RUN/O"
  : > "$LD/facts.jsonl"
  python3 - "$LD/meta.json" <<EOF
import json, sys
json.dump({"leg": "$LEG", "arm": "$ARM", "bucket": "$BUCKET", "endpoint": "$ENDPOINT" or None,
  "region": "$AWS_REGION", "prefix": "$PFX", "run_root": "$RUN", "started_utc": "$TS",
  "writer_bin": "$BIN", "writer_bin_sha256": "$(sha "$BIN")",
  "oracle_bin": "$FSYNC_FIXED", "oracle_bin_sha256": "$(sha "$FSYNC_FIXED")",
  "gateway_bin": "${GATEWAY_BIN:-}", "hold_gc_secs": $HOLD_GC_SECS, "hold_commit_secs": $HOLD_COMMIT_SECS,
  "wait_secs": $WAIT_SECS, "quiesce_rounds": $QUIESCE_ROUNDS, "h5_mode": "$H5_MODE", "h5_big_mb": $H5_BIG_MB},
  open(sys.argv[1], "w"), indent=2)
EOF
  echo "== $LEG $ARM  prefix=$PFX  writers=$BIN"
  if [ "${SKIP_PROBE:-}" = 1 ]; then
    echo "flint-sync: probe-conditional SKIPPED by SKIP_PROBE=1" > "$LD/probe.txt"
  else
    probe_store "$PFX-probe" "$LD/probe.txt" "$RUN/P" || return 1
  fi
  return 0
}

finish_leg() { # quiesce <writers...> is done by the leg; this reads the oracles and judges
  local who
  if [ -e "$RUN/C" ]; then mv "$RUN/C" "$RUN/C.before"; fi
  mkdir -p "$RUN/C"
  fs C oracle-checkout "$FSYNC_FIXED" "$RUN/C" checkout
  echo $? > "$LD/C.checkout.rc"
  manifest "$LD/manifest.final.json" || echo "  manifest read failed (final)"
  for who in "$@" C; do
    py digest "$RUN/$who" > "$LD/digest.$who.txt"
  done
  py verdict "$LEG" "$ARM" "$LD"
}

void_now() { # <reason> — the leg could not start; judged by the verdict code from the facts
  py verdict "$LEG" "$ARM" "$LD"
}

quiesce() { # <writers...> — QUIESCE_ROUNDS rounds, each writer one barrier per round
  local r who rc
  for r in $(seq 1 "$QUIESCE_ROUNDS"); do
    for who in "$@"; do
      fs "$who" "quiesce-$r" "$BIN" "$RUN/$who" barrier; rc=$?
      [ $rc -eq 0 ] || fact "post-rc-$who-$r" false "$who barrier round $r exit $rc" false
    done
  done
}

# ── H1: F1, the GC gap ────────────────────────────────────────────────────
leg_h1() {
  setup_leg H1 || { void_now; return $?; }
  local A=$RUN/A B=$RUN/B rc
  fs A seed-checkout "$BIN" "$A" checkout || fact a-checkout false "exit $?"
  writefile "$A/x.txt" "seed $TS"
  writefile "$A/keep.txt" "keep $TS"
  fs A seed-publish "$BIN" "$A" barrier || fact a-publish false "exit $?"
  fs B seed-checkout "$BIN" "$B" checkout || fact b-checkout false "exit $?"
  [ "$(readfile "$B/x.txt")" = "seed $TS" ] && fact b-has-seed true "" || fact b-has-seed false "B/x.txt=$(readfile "$B/x.txt")"
  rm -f "$A/x.txt"
  fs A first-absence "$BIN" "$A" barrier || fact a-first-absence false "exit $?"
  manifest "$LD/manifest.pre.json"
  [ -n "$(jget "$LD/manifest.pre.json" 'd["entries"].get("x.txt",{}).get("etag")')" ] \
    && fact first-absence-withheld true "" || fact first-absence-withheld false "the manifest no longer cites x.txt after ONE absence"
  writefile "$B/x.txt" "B's edit of x.txt, longer than the seed $TS"
  if grep -q '"ok":false' "$LD/facts.jsonl"; then void_now; return $?; fi

  fs_bg A race-a "$BIN" "$A" barrier FLINT_SYNC_DRILL_HOLD_GC_SECS="$HOLD_GC_SECS"; local apid=$BG_PID
  await A race-a "$apid" "$WAIT_SECS" ev=drill_hold where=gc path=x.txt; rc=$?
  if [ $rc -ne 0 ]; then
    fact a-gc-hold false "A never logged drill_hold on x.txt (wait rc=$rc)"
    bg_wait A race-a "$apid" "$WAIT_SECS"; void_now; return $?
  fi
  # A has HEADed x.txt at the seed etag and sleeps before its DELETE. B's
  # upload holds no lease: it lands now, then its claim queues behind A.
  fs_bg B race-b "$BIN" "$B" barrier; local bpid=$BG_PID
  await B race-b "$bpid" "$((HOLD_GC_SECS + WAIT_SECS))" ev=upload path=x.txt outcome=put \
    || echo "  B's PUT of x.txt was not seen (the guard decides)"
  bg_wait A race-a "$apid" "$((HOLD_GC_SECS + WAIT_SECS))"; fact a-race-exit true "rc=$?" false
  bg_wait B race-b "$bpid" "$((HOLD_GC_SECS + WAIT_SECS))"; fact b-race-exit true "rc=$?" false
  manifest "$LD/manifest.race.json"
  quiesce A B
  finish_leg A B
}

# ── H2: F2, the adopt window ──────────────────────────────────────────────
leg_h2() {
  setup_leg H2 || { void_now; return $?; }
  local A=$RUN/A B=$RUN/B rc
  fs A seed-checkout "$BIN" "$A" checkout || fact a-checkout false "exit $?"
  writefile "$A/x.txt" "seed $TS"
  writefile "$A/keep.txt" "keep $TS"
  fs A seed-publish "$BIN" "$A" barrier || fact a-publish false "exit $?"
  fs B seed-checkout "$BIN" "$B" checkout || fact b-checkout false "exit $?"
  local SAME="the same bytes, written by both writers $TS"
  writefile "$B/x.txt" "$SAME"
  fs B publish-same "$BIN" "$B" barrier || fact b-publish-same false "exit $?"
  # A has not integrated B's boundary: its base for x.txt is still the seed.
  writefile "$A/x.txt" "$SAME"
  rm -f "$B/x.txt"
  fs B first-absence "$BIN" "$B" barrier || fact b-first-absence false "exit $?"
  manifest "$LD/manifest.pre.json"
  local cited; cited=$(jget "$LD/manifest.pre.json" 'norm(d["entries"].get("x.txt",{}).get("etag"))')
  [ -n "$cited" ] && fact first-absence-withheld true "cites $cited" || fact first-absence-withheld false "x.txt uncited after ONE absence"
  if grep -q '"ok":false' "$LD/facts.jsonl"; then void_now; return $?; fi

  fs_bg B race-b "$BIN" "$B" barrier FLINT_SYNC_DRILL_HOLD_COMMIT_SECS="$HOLD_COMMIT_SECS"; local bpid=$BG_PID
  await B race-b "$bpid" "$WAIT_SECS" prose "DRILL: holding the fence"; rc=$?
  if [ $rc -ne 0 ]; then
    fact b-commit-hold false "B never reached its held commit section (wait rc=$rc)"
    bg_wait B race-b "$bpid" "$WAIT_SECS"; void_now; return $?
  fi
  # B holds the fence and has uncited nothing yet. A's upload 412s on the
  # seed, finds B's identical bytes, ADOPTS them, and waits for the fence;
  # B then commits its delete and its GC collects the object A adopted.
  fs_bg A race-a "$BIN" "$A" barrier; local apid=$BG_PID
  await A race-a "$apid" "$((HOLD_COMMIT_SECS + WAIT_SECS))" ev=upload path=x.txt outcome=adopted \
    || echo "  A's adopt of x.txt was not seen (the guard decides)"
  bg_wait B race-b "$bpid" "$((HOLD_COMMIT_SECS + WAIT_SECS))"; fact b-race-exit true "rc=$?" false
  bg_wait A race-a "$apid" "$((HOLD_COMMIT_SECS + WAIT_SECS))"; fact a-race-exit true "rc=$?" false
  manifest "$LD/manifest.race.json"
  quiesce A B
  finish_leg A B
}

# ── H3: F3, the sync overlay ──────────────────────────────────────────────
GW_PID=
gateway_start() {
  [ -n "${GATEWAY_BIN:-}" ] && [ -x "$GATEWAY_BIN" ] || { fact gateway-bin false "GATEWAY_BIN is not an executable"; return 1; }
  mark G gateway start
  env -u FLINT_SYNC_ENDPOINT -u FLINT_LEAN_GW_ENDPOINT "${STORE_ENV[@]}" \
    FLINT_LEAN_GW_LISTEN="127.0.0.1:$GW_PORT" FLINT_LEAN_GW_BUCKET="$BUCKET" \
    FLINT_LEAN_GW_TOKEN="$GW_TOKEN" FLINT_LEAN_GW_WORKSPACES="h3=$PFX" \
    "$GATEWAY_BIN" >> "$LD/G.log" 2>&1 &
  GW_PID=$!
  local i
  for i in $(seq 1 $((WAIT_SECS * 5))); do
    alive "$GW_PID" || { fact gateway-up false "the gateway exited: $(tail -2 "$LD/G.log" | tr '\n' ' ')"; return 1; }
    curl -s -o /dev/null "http://127.0.0.1:$GW_PORT/healthz" && return 0
    sleep 0.2
  done
  fact gateway-up false "no /healthz within ${WAIT_SECS}s"; return 1
}
gateway_stop() {
  [ -n "$GW_PID" ] || return 0
  kill "$GW_PID" 2>/dev/null; wait "$GW_PID" 2>/dev/null
  mark G gateway "exit rc=stopped"; GW_PID=
}

leg_h3() {
  setup_leg H3 || { void_now; return $?; }
  local A=$RUN/A B=$RUN/B rc code etag
  fs A seed-checkout "$BIN" "$A" checkout || fact a-checkout false "exit $?"
  writefile "$A/x.txt" "seed $TS"
  writefile "$A/keep.txt" "keep $TS"
  fs A seed-publish "$BIN" "$A" barrier || fact a-publish false "exit $?"
  fs B seed-checkout "$BIN" "$B" checkout || fact b-checkout false "exit $?"
  rm -f "$A/x.txt"
  fs A first-absence "$BIN" "$A" barrier || fact a-first-absence false "exit $?"
  manifest "$LD/manifest.pre.json"
  etag=$(jget "$LD/manifest.pre.json" 'norm(d["entries"].get("x.txt",{}).get("etag"))')
  [ -n "$etag" ] && fact first-absence-withheld true "cites $etag" || fact first-absence-withheld false "x.txt uncited after ONE absence"
  if grep -q '"ok":false' "$LD/facts.jsonl"; then void_now; return $?; fi

  # The UI's write through the gateway: the object, then its inbox entry.
  local HITL="written in the UI through the gateway $TS"
  if gateway_start; then
    code=$(curl -sS -o "$LD/ui.put.json" -w '%{http_code}' -X PUT \
      -H "Authorization: Bearer $GW_TOKEN" -H "If-Match: \"$etag\"" -H "x-flint-author: hostlegs" \
      --data-binary "$HITL" "http://127.0.0.1:$GW_PORT/lean/v1/h3/files/x.txt" 2>> "$LD/G.log")
    [ "$code" = 200 ] && fact ui-write true "200 $(cat "$LD/ui.put.json")" \
      || fact ui-write false "HTTP $code $(cat "$LD/ui.put.json" 2>/dev/null)"
  fi
  gateway_stop
  if grep -q '"ok":false' "$LD/facts.jsonl"; then void_now; return $?; fi

  # A publishes the delete (its consume keeps the local delete and preserves
  # the UI bytes); held between the GC's HEAD and DELETE — after the CAS
  # uncited x.txt and before the window clear drops the inbox entry.
  fs_bg A race-a "$BIN" "$A" barrier FLINT_SYNC_DRILL_HOLD_GC_SECS="$HOLD_GC_SECS"; local apid=$BG_PID
  await A race-a "$apid" "$WAIT_SECS" ev=drill_hold where=gc path=x.txt; rc=$?
  if [ $rc -ne 0 ]; then
    fact a-gc-hold false "A never logged drill_hold on x.txt (wait rc=$rc)"
    bg_wait A race-a "$apid" "$WAIT_SECS"; void_now; return $?
  fi
  fs B race-sync "$BIN" "$B" sync; rc=$?
  fact b-sync-exit "$([ $rc -eq 0 ] && echo true || echo false)" "rc=$rc"
  [ "$(readfile "$B/x.txt")" = "$HITL" ] && fact b-applied-hitl true "" \
    || fact b-applied-hitl false "B/x.txt after sync: $(readfile "$B/x.txt" | head -c 80)"
  bg_wait A race-a "$apid" "$((HOLD_GC_SECS + WAIT_SECS))"; fact a-race-exit true "rc=$?" false
  manifest "$LD/manifest.race.json"
  quiesce B A
  finish_leg A B
}

# ── H5: finding 10 (OPEN) — a writer killed between its upload and its CAS ─
leg_h5() {
  setup_leg H5 || { void_now; return $?; }
  local A=$RUN/A B=$RUN/B rc i
  fs A seed-checkout "$BIN" "$A" checkout || fact a-checkout false "exit $?"
  writefile "$A/p1.txt" "seed-1 $TS"
  writefile "$A/p2.bin" "seed-2 $TS"
  fs A seed-publish "$BIN" "$A" barrier || fact a-publish false "exit $?"
  fs B seed-checkout "$BIN" "$B" checkout || fact b-checkout false "exit $?"
  manifest "$LD/manifest.pre.json"
  if grep -q '"ok":false' "$LD/facts.jsonl"; then void_now; return $?; fi

  writefile "$A/p1.txt" "A's unpublished edit of p1 $TS"
  local apid
  if [ "$H5_MODE" = mid-upload ]; then
    dd if=/dev/urandom of="$A/p2.bin" bs=1048576 count="$H5_BIG_MB" 2>/dev/null
    fs_bg A race-a "$BIN" "$A" barrier FLINT_SYNC_UPLOAD_FANOUT=1; apid=$BG_PID
    local c1; c1=$(jget "$LD/manifest.pre.json" 'norm(d["entries"]["p1.txt"]["etag"])')
    rc=1
    for i in $(seq 1 $((WAIT_SECS * 10))); do
      alive "$apid" || break
      manifest "$LD/manifest.poll.json" || continue
      if [ "$(jget "$LD/manifest.poll.json" 'norm(d["heads"].get("p1.txt"))')" != "$c1" ]; then rc=0; break; fi
    done
    [ $rc -eq 0 ] || fact a-p1-landed false "p1.txt's key never moved while A ran"
  else
    writefile "$A/p2.bin" "A's other edit $TS"
    fs_bg A race-a "$BIN" "$A" barrier FLINT_SYNC_DRILL_HOLD_COMMIT_SECS=600; apid=$BG_PID
    await A race-a "$apid" "$WAIT_SECS" prose "DRILL: holding the fence"; rc=$?
    [ $rc -eq 0 ] || fact a-commit-hold false "A never reached its held commit section (wait rc=$rc)"
  fi
  kill -9 "$apid" 2>/dev/null
  wait "$apid" 2>/dev/null; rc=$?
  mark A race-a "exit rc=$rc"
  [ $rc -eq 137 ] && fact a-killed true "rc=137" || fact a-killed false "A exit rc=$rc (not the kill)"
  manifest "$LD/manifest.at_kill.json"
  # Pod replacement: the state directory goes with the pod; A never returns.
  rm -rf "$A/.flint-sync"
  mark A pod-replaced "note state dir removed; A is gone for good"
  if grep -q '"ok":false' "$LD/facts.jsonl"; then void_now; return $?; fi

  writefile "$B/b.txt" "B works elsewhere $TS"
  quiesce B
  finish_leg B
}

# ── fakes3, local only ───────────────────────────────────────────────────
fakes3_cmd() {
  local bin=${FAKES3_BIN:-$HERE/../perf/fakes3/target/release/fakes3}
  local listen=${ENDPOINT#http://}; listen=${listen%/}
  [ -n "$ENDPOINT" ] || die "ENDPOINT=http://127.0.0.1:<port> is required for fakes3"
  case "${1:-}" in
    start)
      [ -x "$bin" ] || die "no fakes3 at $bin (cargo build --release in lean/e2e/perf/fakes3)"
      mkdir -p "$ROOT/fakes3-seed/_hostlegs"
      printf 'fakes3 refuses an empty store\n' > "$ROOT/fakes3-seed/_hostlegs/seed"
      printf '_hostlegs/seed\thostlegs-seed-etag\n' > "$ROOT/fakes3-etags.tsv"
      nohup "$bin" --seed-dir "$ROOT/fakes3-seed" --etags "$ROOT/fakes3-etags.tsv" --listen "$listen" \
        > "$ROOT/fakes3.log" 2>&1 &
      echo $! > "$ROOT/fakes3.pid"
      local i
      for i in $(seq 1 50); do
        curl -s -o /dev/null "$ENDPOINT/__stats" && { echo "fakes3 up on $ENDPOINT (pid $(cat "$ROOT/fakes3.pid"))"; return 0; }
        sleep 0.2
      done
      die "fakes3 did not come up: $(tail -3 "$ROOT/fakes3.log")"
      ;;
    stop)
      [ -f "$ROOT/fakes3.pid" ] && kill "$(cat "$ROOT/fakes3.pid")" 2>/dev/null; rm -f "$ROOT/fakes3.pid"; echo "fakes3 stopped"
      ;;
    *) die "fakes3 start|stop" ;;
  esac
}

case "$CMD" in
  H1) leg_h1 ;;
  H2) leg_h2 ;;
  H3) leg_h3 ;;
  H5) leg_h5 ;;
  probe)
    store_env
    [ -n "${FSYNC_FIXED:-}" ] && [ -x "$FSYNC_FIXED" ] || die "FSYNC_FIXED must name an executable flint-sync"
    mkdir -p "$OUT"
    PFX="$PREFIX_BASE/probe-$(date -u +%Y%m%dT%H%M%SZ)"
    probe_store "$PFX" "$OUT/probe.txt" "$ROOT/probe-root-${PFX##*/}"
    ;;
  selftest) py selftest ;;
  fakes3) fakes3_cmd "$ARM" ;;
  *) sed -n '2,62p' "$0"; exit 2 ;;
esac
