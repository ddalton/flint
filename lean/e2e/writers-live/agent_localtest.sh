#!/usr/bin/env bash
# agent_localtest.sh — agent.sh and ui.sh, run for real on this machine.
#
# No syncer, no cluster: a python FAKE SYNCER watches each agent tree's
# .flint/publish and answers .flint/publish.ack (compact or pretty JSON,
# sometimes `partial` with a dropped path, seq sometimes absent, a conflict
# record with a hostile string, and one publish per agent never answered on
# its own so the agent journals `no-ack` and a later ack covers both
# nonces). A python FAKE GATEWAY serves ui.sh (If-Match semantics, a 412
# from a simulated concurrent write, one 409 window). Then the journals are
# checked against README §1: they parse, op lines precede their ack, one op
# per path per batch, `base`/`sha256` are exactly what the tree held (the
# ops are replayed over a model tree, which must end equal to the disk), the
# ack fields are what the fake wrote, pause idles, stop exits 0.
#
# Also: the ack parser against fixture acks, the LCG, and the base-read race
# check (a "consume" injected between the hash and the rename must be seen).
#
#   agent_localtest.sh            (env: AGENT_SH, UI_SH, SHELL_UNDER_TEST, KEEP=1)
#
# It does NOT judge convergence — there is no sync here.
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
AGENT_SH=${AGENT_SH:-$HERE/agent.sh}
UI_SH=${UI_SH:-$HERE/ui.sh}
SHELL_UNDER_TEST=${SHELL_UNDER_TEST:-$(command -v dash || echo /bin/sh)}
WORK=$(mktemp -d "${TMPDIR:-/tmp}/agent-localtest.XXXXXX")
PIDS=()
FAILS=0

cleanup() {
    for p in "${PIDS[@]:-}"; do [ -n "$p" ] && kill "$p" 2>/dev/null || true; done
    if [ "${KEEP:-0}" = 1 ]; then echo "kept: $WORK"; else rm -rf "$WORK"; fi
}
trap cleanup EXIT

check() { # name, then a command
    local name=$1; shift
    if "$@"; then echo "  PASS  $name"; else echo "  FAIL  $name"; FAILS=$((FAILS + 1)); fi
}

echo "shell under test: $SHELL_UNDER_TEST"
echo "agent: $AGENT_SH"

# ---------------------------------------------------------------------------
echo "== 1. ack parser (agent.sh parse-ack) on compact, pretty and hostile acks"
mkdir -p "$WORK/parse"
python3 - "$WORK/parse" <<'PY'
import json, sys, os
d = sys.argv[1]
hostile = {"path": "x]", "foreign_etag": "\"e\"", "preserved_key": None,
           "kind": "weird ] \"dropped\": [\"no\"], \"nonces\": [\"zz\"], \"seq\": 7", "at_unix": 1}
base = {"status": "partial", "nonces": ["a3-8", "a3-9"], "sentinel_mtime_unix_ns": 123, "seq": 412,
        "manifest_etag": "\"abc\"", "boundary": "sentinel", "completed_unix": 1757800004,
        "report": {"uploaded": 3, "deleted": 1, "parked": 1, "consumed": 2, "no_change": False,
                   "conflicts": [hostile], "out_of_scope_foreign": 0, "dropped": ["hot/p03.txt"]}}
cases = {}
cases["compact"] = (json.dumps(base, separators=(",", ":")),
    'status=partial\nseq=412\nnonces=["a3-8","a3-9"]\ndropped=["hot/p03.txt"]\nuploaded=3\ndeleted=1\nparked=1\nconsumed=2\nno_change=false\n')
cases["pretty"] = (json.dumps(base, indent=2), cases["compact"][1])
b2 = json.loads(json.dumps(base)); b2["status"] = "ok"; b2["nonces"] = ["a1-1"]; del b2["seq"]
del b2["report"]["dropped"]; b2["report"]["no_change"] = True; b2["report"]["parked"] = 0
cases["pretty-no-seq-no-dropped-hostile"] = (json.dumps(b2, indent=2),
    'status=ok\nseq=null\nnonces=["a1-1"]\ndropped=[]\nuploaded=3\ndeleted=1\nparked=0\nconsumed=2\nno_change=true\n')
b3 = json.loads(json.dumps(b2)); b3["nonces"] = []; del b3["report"]["conflicts"]
cases["compact-empty-nonces"] = (json.dumps(b3, separators=(",", ":")),
    'status=ok\nseq=null\nnonces=[]\ndropped=[]\nuploaded=3\ndeleted=1\nparked=0\nconsumed=2\nno_change=true\n')
for name, (text, want) in cases.items():
    open(os.path.join(d, name + ".json"), "w").write(text + "\n")
    open(os.path.join(d, name + ".want"), "w").write(want)
PY
for f in "$WORK"/parse/*.json; do
    want=${f%.json}.want
    "$SHELL_UNDER_TEST" "$AGENT_SH" parse-ack "$f" > "${f%.json}.got" 2>&1 || true
    check "parse-ack $(basename "${f%.json}")" cmp -s "$want" "${f%.json}.got"
done

# ---------------------------------------------------------------------------
echo "== 2. the LCG is seeded and deterministic"
"$SHELL_UNDER_TEST" "$AGENT_SH" rand 7 300 > "$WORK/r7a"
"$SHELL_UNDER_TEST" "$AGENT_SH" rand 7 300 > "$WORK/r7b"
"$SHELL_UNDER_TEST" "$AGENT_SH" rand 8 300 > "$WORK/r8"
check "same seed, same sequence" cmp -s "$WORK/r7a" "$WORK/r7b"
check "different seed, different sequence" bash -c "! cmp -s '$WORK/r7a' '$WORK/r8'"
check "draws in [0,100), every decile hit" python3 -c "
import sys; v=[int(x) for x in open('$WORK/r7a')]
assert all(0 <= x < 100 for x in v); assert len({x//10 for x in v}) == 10; assert len(set(v)) > 50"

# ---------------------------------------------------------------------------
echo "== 3. the base read sees a consume injected between hash and rename"
mkdir -p "$WORK/race/tree/.flint" "$WORK/race/ctl"
cat > "$WORK/race/race.sh" <<'SH'
set -u
AGENT_LIB_ONLY=1
. "$AGENT_SH"
AGENT_ID=r1; AGENT_MODE=hot; AGENT_SEED=1; RACE_TRIES=5
TREE=$RACE_TREE; JOURNAL=$RACE_JOURNAL; N=0; BATCH=1; NONCE=r1-1
detect_time; detect_sha; seed_lcg 1 0
mkdir -p "$TREE/hot"
CALLS=0; HOOK_AT=0; HOOK_KIND=none
ino_of() {        # the real ino_of, plus a "consume" at call HOOK_AT
    _hp=$1
    CALLS=$((CALLS + 1))
    if [ "$CALLS" = "$HOOK_AT" ] && [ "$HOOK_KIND" = replace ]; then
        printf 'consumed\n' > "$_hp.hook$TMP_SUFFIX"; mv -f "$_hp.hook$TMP_SUFFIX" "$_hp"
    fi
    INO=""
    set -- $(ls -di "$_hp" 2>/dev/null)
    INO=${1:-}
    if [ "$CALLS" = "$HOOK_AT" ] && [ "$HOOK_KIND" = create-after ]; then
        printf 'created\n' > "$_hp.hook$TMP_SUFFIX"; mv -f "$_hp.hook$TMP_SUFFIX" "$_hp"
    fi
}
printf 'old\n' > "$TREE/hot/p00.txt"; CALLS=0; HOOK_AT=2; HOOK_KIND=replace
NONCE=race-overwrite; op_write hot/p00.txt
CALLS=0; HOOK_AT=1; HOOK_KIND=create-after
NONCE=race-create; op_write hot/p01.txt
printf 'old\n' > "$TREE/hot/p02.txt"; CALLS=0; HOOK_AT=2; HOOK_KIND=replace
NONCE=race-delete; op_delete hot/p02.txt
printf 'old\n' > "$TREE/hot/p03.txt"; CALLS=0; HOOK_AT=3; HOOK_KIND=replace
NONCE=race-mv; op_mv hot/p03.txt hot/p04.txt
printf 'old\n' > "$TREE/hot/p05.txt"; CALLS=0; HOOK_AT=0; HOOK_KIND=none
NONCE=race-none; op_write hot/p05.txt
SH
AGENT_SH="$AGENT_SH" RACE_TREE="$WORK/race/tree" RACE_JOURNAL="$WORK/race/journal.jsonl" \
    "$SHELL_UNDER_TEST" "$WORK/race/race.sh" 2> "$WORK/race/stderr" || true
python3 - "$WORK/race" <<'PY' > "$WORK/race/verdict" || true
import hashlib, json, os, sys
d = sys.argv[1]; t = os.path.join(d, "tree")
h = lambda b: hashlib.sha256(b).hexdigest()
ops = {}
for l in open(os.path.join(d, "journal.jsonl")):
    o = json.loads(l); ops.setdefault(o["nonce"], []).append(o)
def one(n):
    v = [o for o in ops.get(n, []) if o["k"] == "op"]; return v[0] if len(v) == 1 else None
def filesha(p):
    p = os.path.join(t, p); return h(open(p, "rb").read()) if os.path.exists(p) else "absent"
res = []
o = one("race-overwrite")
res.append(("overwrite: base is the consumed bytes", bool(o) and o["base"] == h(b"consumed\n") and filesha("hot/p00.txt") == o["sha256"]))
o = one("race-create")
res.append(("create: a file that appeared is not replaced blind", bool(o) and o["base"] == h(b"created\n") and filesha("hot/p01.txt") == o["sha256"]))
o = one("race-delete")
res.append(("delete: base is the consumed bytes", bool(o) and o["base"] == h(b"consumed\n") and filesha("hot/p02.txt") == "absent"))
o = one("race-mv")
res.append(("mv: base and moved sha are the consumed bytes", bool(o) and o["base"] == h(b"consumed\n") and o["sha256"] == h(b"consumed\n") and filesha("hot/p04.txt") == h(b"consumed\n") and filesha("hot/p03.txt") == "absent"))
o = one("race-none")
res.append(("control, no consume: base is the old bytes", bool(o) and o["base"] == h(b"old\n")))
leftover = [f for r, _, fs in os.walk(t) for f in fs if f.endswith(".flint-sync-tmp")]
res.append(("no temp left behind", not leftover))
for name, ok in res:
    print(("PASS " if ok else "FAIL ") + name)
PY
while read -r verdict name; do
    check "race: $name" test "$verdict" = PASS
done < "$WORK/race/verdict"
[ -s "$WORK/race/verdict" ] || check "race: verdict produced ($(tail -3 "$WORK/race/stderr" | tr '\n' ' '))" false

# ---------------------------------------------------------------------------
echo "== 4. three agents against a fake syncer"
mkdir -p "$WORK/fake"
cat > "$WORK/fake/syncer.py" <<'PY'
import json, os, random, sys, time
work = sys.argv[1]
trees = dict(a.split("=", 1) for a in sys.argv[2:])
rng = random.Random(4242)
WITHHOLD = {"a1": {4}, "a2": {3}}       # publish ordinals never answered on their own
st = {a: {"pending": [], "count": 0, "due": None, "acks": 0, "mtime": 0} for a in trees}
seq = 100
glob_acks = 0
logs = {a: open(os.path.join(work, f"acks-{a}.jsonl"), "a") for a in trees}
while not os.path.exists(os.path.join(work, "stop")):
    now = time.time()
    for a, tree in trees.items():
        s = st[a]
        pub = os.path.join(tree, ".flint", "publish")
        if os.path.exists(pub):
            try:
                body = open(pub).read()
                os.replace(pub, os.path.join(tree, ".flint", "publish.consumed"))
            except FileNotFoundError:
                continue
            s["count"] += 1
            s["mtime"] = time.time_ns()
            s["pending"].append(json.loads(body)["nonce"])
            s["due"] = None if s["count"] in WITHHOLD.get(a, ()) else now + rng.uniform(0.05, 0.4)
        if s["pending"] and s["due"] is not None and now >= s["due"]:
            s["acks"] += 1; glob_acks += 1; seq += 1
            files = sorted(os.path.relpath(os.path.join(r, f), tree) for r, ds, fs in os.walk(tree)
                           for f in fs if not os.path.relpath(os.path.join(r, f), tree).startswith(".flint")
                           and not f.endswith(".flint-sync-tmp"))
            dropped = files[:1] if s["acks"] % 4 == 0 else []
            report = {"uploaded": rng.randint(0, 5), "deleted": rng.randint(0, 2), "parked": len(dropped),
                      "consumed": rng.randint(0, 3), "no_change": rng.random() < 0.2}
            if s["acks"] % 3 == 0:
                report["conflicts"] = [{"path": "hot/x.txt", "foreign_etag": "\"e\"", "preserved_key": None,
                                        "kind": "weird ] \"dropped\": [\"no\"], \"nonces\": [\"zz\"]", "at_unix": 1}]
            report["out_of_scope_foreign"] = 0
            if dropped:
                report["dropped"] = dropped
            ack = {"status": "partial" if dropped else "ok", "nonces": list(s["pending"]),
                   "sentinel_mtime_unix_ns": s["mtime"]}
            if glob_acks % 7 != 0:
                ack["seq"] = seq
            ack.update({"manifest_etag": f"\"m{seq}\"", "boundary": "sentinel", "completed_unix": int(now),
                        "report": report})
            pretty = s["acks"] % 2 == 1
            text = json.dumps(ack, indent=2) if pretty else json.dumps(ack, separators=(",", ":"))
            tmp = os.path.join(tree, ".flint", "publish.ack.tmp")
            open(tmp, "w").write(text)
            os.replace(tmp, os.path.join(tree, ".flint", "publish.ack"))
            logs[a].write(json.dumps({"ack": ack, "pretty": pretty}) + "\n"); logs[a].flush()
            s["pending"] = []; s["due"] = None
    time.sleep(0.02)
PY
SPECS=("a1 churn 11" "a2 vocab 12" "a3 disjoint 13")
TREE_ARGS=()
for spec in "${SPECS[@]}"; do
    read -r id mode seed <<< "$spec"
    mkdir -p "$WORK/trees/$id/.flint" "$WORK/agents/$id/ctl"
    TREE_ARGS+=("$id=$WORK/trees/$id")
done
python3 "$WORK/fake/syncer.py" "$WORK/fake" "${TREE_ARGS[@]}" 2> "$WORK/fake/stderr" &
PIDS+=($!)
disown "$!"
START_MS=$(python3 -c 'import time; print(int(time.time()*1000))')
for spec in "${SPECS[@]}"; do
    read -r id mode seed <<< "$spec"
    AGENT_ID=$id AGENT_MODE=$mode AGENT_SEED=$seed TREE="$WORK/trees/$id" \
    JOURNAL="$WORK/agents/$id/journal.jsonl" CONTROL_DIR="$WORK/agents/$id/ctl" \
    OPS_PER_BATCH=6 BATCH_SLEEP_MIN_MS=100 BATCH_SLEEP_MAX_MS=300 FLOOR_SECS=1 ACK_POLL_MS=50 \
    AGENT_PATHS=12 READY_TIMEOUT_SECS=10 \
        "$SHELL_UNDER_TEST" "$AGENT_SH" 2> "$WORK/agents/$id/stderr" &
    PIDS+=($!)
    echo "$!" > "$WORK/agents/$id/pid"
done

acks_of() { local n; n=$(grep -c '"k":"ack"' "$WORK/agents/$1/journal.jsonl" 2>/dev/null) || true; echo "${n:-0}"; }
deadline=$((SECONDS + 60))
until [ "$(acks_of a1)" -ge 14 ] && [ "$(acks_of a2)" -ge 14 ] && [ "$(acks_of a3)" -ge 10 ]; do
    if [ "$SECONDS" -ge "$deadline" ]; then echo "  agents did not reach 14 acks in 60s"; break; fi
    sleep 0.5
done

# pause: a3 finishes its batch and idles; nothing is journaled until resumed
touch "$WORK/agents/a3/ctl/pause"
deadline=$((SECONDS + 20))
until grep -q '"k":"paused"' "$WORK/agents/a3/journal.jsonl"; do
    [ "$SECONDS" -ge "$deadline" ] && break; sleep 0.2
done
lines_paused=$(wc -l < "$WORK/agents/a3/journal.jsonl")
sleep 2.5
lines_later=$(wc -l < "$WORK/agents/a3/journal.jsonl")
check "pause: a3 journaled 'paused' and nothing for 2.5s" test "$lines_paused" -eq "$lines_later"
rm -f "$WORK/agents/a3/ctl/pause"
deadline=$((SECONDS + 10))
until grep -q '"k":"resumed"' "$WORK/agents/a3/journal.jsonl"; do
    [ "$SECONDS" -ge "$deadline" ] && break; sleep 0.2
done
sleep 1

for spec in "${SPECS[@]}"; do read -r id _ <<< "$spec"; touch "$WORK/agents/$id/ctl/stop"; done
for spec in "${SPECS[@]}"; do
    read -r id _ <<< "$spec"
    pid=$(cat "$WORK/agents/$id/pid"); rc=0
    for _ in $(seq 1 150); do kill -0 "$pid" 2>/dev/null || break; sleep 0.2; done
    if kill -0 "$pid" 2>/dev/null; then rc=timeout; kill "$pid" 2>/dev/null || true; else wait "$pid" || rc=$?; fi
    check "stop: $id exited 0 (rc=$rc)" test "$rc" = 0
done
touch "$WORK/fake/stop"
END_MS=$(python3 -c 'import time; print(int(time.time()*1000))')

cat > "$WORK/validate.py" <<'PY'
import hashlib, json, os, sys
work, start_ms, end_ms = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
H = lambda b: hashlib.sha256(b).hexdigest()
VOCAB = {H(("writers-live vocab body %d\n" % k + "".join("line %d of body %d\n" % (i, k) for i in range(k + 1))).encode())
         for k in range(8)}
MODES = {"a1": "churn", "a2": "vocab", "a3": "disjoint"}
results = []
def check(name, ok, detail=""):
    results.append((name, bool(ok), detail))

seen_content = {}
agg = {"no-ack": 0, "coalesced": 0, "partial": 0, "seq-null": 0, "pretty": 0, "compact": 0}
for a, mode in MODES.items():
    jp = os.path.join(work, "agents", a, "journal.jsonl")
    raw = open(jp).read().splitlines()
    try:
        lines = [json.loads(l) for l in raw]
        check(f"{a}: every journal line is JSON", True)
    except ValueError as e:
        check(f"{a}: every journal line is JSON", False, str(e)); continue
    kinds = {l.get("k") for l in lines}
    check(f"{a}: line kinds known", kinds <= {"start", "op", "ack", "skip", "paused", "resumed", "stopped"}, kinds)
    check(f"{a}: starts with start, ends with stopped", lines[0]["k"] == "start" and lines[-1]["k"] == "stopped")
    ts = [l["t_ms"] for l in lines]
    check(f"{a}: t_ms non-decreasing, inside the run", ts == sorted(ts) and start_ms - 1000 <= ts[0] and ts[-1] <= end_ms + 1000,
          (ts[0], ts[-1], start_ms, end_ms))
    check(f"{a}: no skipped ops (nothing races here)", "skip" not in kinds)
    # batch structure
    ack_at, ops_by_nonce, order_ok, dup_paths, acks = {}, {}, True, [], []
    for i, l in enumerate(lines):
        if l["k"] == "op":
            if l["nonce"] in ack_at: order_ok = False
            ops_by_nonce.setdefault(l["nonce"], []).append(l)
        elif l["k"] == "ack":
            if l["nonce"] in ack_at: order_ok = False
            ack_at[l["nonce"]] = i; acks.append(l)
    check(f"{a}: op lines precede their ack; one ack per nonce", order_ok and all(n in ack_at for n in ops_by_nonce))
    batches = [int(l["nonce"].rsplit("-", 1)[1]) for l in acks]
    check(f"{a}: nonces are <agent>-<batch>, strictly increasing", batches == sorted(set(batches))
          and all(l["nonce"].startswith(a + "-") for l in acks))
    for n, ops in ops_by_nonce.items():
        touched = [o["path"] for o in ops] + [o["to"] for o in ops if o["op"] == "mv"]
        if len(touched) != len(set(touched)): dup_paths.append(n)
    check(f"{a}: a path at most once per batch", not dup_paths, dup_paths)
    # replay the ops over a model tree
    model, bad = {}, []
    counts = {"write": 0, "delete": 0, "mv": 0, "same": 0}
    for l in lines:
        if l["k"] != "op": continue
        p = l["path"]
        if l["op"] == "write":
            if l["base"] != model.get(p, "absent"): bad.append(("base", l))
            if l["sha256"] == l["base"]: counts["same"] += 1
            else:
                counts["write"] += 1
                if mode != "vocab":
                    if l["sha256"] in seen_content: bad.append(("content not unique", l))
                    seen_content[l["sha256"]] = (a, l["n"])
            if mode == "vocab" and l["sha256"] not in VOCAB: bad.append(("not a vocab body", l))
            model[p] = l["sha256"]
        elif l["op"] == "delete":
            counts["delete"] += 1
            if l["base"] != model.get(p, "absent") or l["base"] == "absent": bad.append(("delete base", l))
            model.pop(p, None)
        elif l["op"] == "mv":
            counts["mv"] += 1
            if l["base"] != model.get(p, "absent") or l["sha256"] != l["base"]: bad.append(("mv base", l))
            if l.get("to_base") != model.get(l["to"], "absent"): bad.append(("mv to_base", l))
            model[l["to"]] = model.pop(p, None)
        if mode == "disjoint" and not p.startswith(a + "/"): bad.append(("disjoint path", l))
    check(f"{a}: base/sha256 replay exactly over a model tree", not bad, bad[:3])
    tree = os.path.join(work, "trees", a)
    disk, leftovers = {}, []
    for r, ds, fs in os.walk(tree):
        for f in fs:
            rel = os.path.relpath(os.path.join(r, f), tree)
            if rel.split(os.sep)[0] == ".flint": continue
            if f.endswith(".flint-sync-tmp"): leftovers.append(rel); continue
            disk[rel] = H(open(os.path.join(r, f), "rb").read())
    check(f"{a}: the replayed model equals the tree on disk", disk == model,
          {"only_disk": sorted(set(disk) - set(model)), "only_model": sorted(set(model) - set(disk))})
    check(f"{a}: no temp files left in the tree", not leftovers, leftovers)
    if mode == "churn":
        check(f"{a}: churn exercised write, delete, mv and same-bytes", all(counts[k] > 0 for k in counts), counts)
    # ack fields == what the fake syncer wrote
    fake = [json.loads(l) for l in open(os.path.join(work, "fake", f"acks-{a}.jsonl"))]
    for f in fake: agg["pretty" if f["pretty"] else "compact"] += 1
    mism = []
    for l in acks:
        if l["status"] == "no-ack":
            agg["no-ack"] += 1
            if any(l["nonce"] in f["ack"]["nonces"] and l["nonce"] == f["ack"]["nonces"][-1] for f in fake):
                mism.append(("no-ack for a nonce the fake answered on its own", l["nonce"]))
            continue
        f = next((f["ack"] for f in fake if l["nonce"] in f["ack"]["nonces"]), None)
        if f is None: mism.append(("no fake ack names", l["nonce"])); continue
        want = {"status": f["status"], "seq": f.get("seq"), "dropped": f["report"].get("dropped", []),
                "covered": f["nonces"], "report": {k: f["report"][k] for k in ("uploaded", "deleted", "parked", "consumed", "no_change")}}
        got = {k: l.get(k) for k in want}
        if got != want: mism.append((l["nonce"], got, want))
        if len(l["covered"]) >= 2: agg["coalesced"] += 1
        if l["status"] == "partial": agg["partial"] += 1
        if l["seq"] is None: agg["seq-null"] += 1
    check(f"{a}: every ack line is exactly the fake's ack (status, seq, dropped, covered, report)", not mism, mism[:3])
    if a == "a3":
        i0 = next((i for i, l in enumerate(lines) if l["k"] == "paused"), None)
        i1 = next((i for i, l in enumerate(lines) if l["k"] == "resumed"), None)
        check("a3: nothing between paused and resumed; the batch before it was acked",
              i0 is not None and i1 == i0 + 1 and lines[i0 - 1]["k"] == "ack", (i0, i1))
    last_acks = [l for l in acks]
    check(f"{a}: stop published once more and journaled its ack", last_acks[-1]["status"] in ("ok", "partial")
          and lines[-2]["k"] == "ack" and not ops_by_nonce.get(last_acks[-1]["nonce"]))
for k, v in agg.items():
    check(f"exercised: {k} ({v})", v > 0)
for name, ok, detail in results:
    print(("  PASS  " if ok else "  FAIL  ") + name + ("" if ok else f"  {detail}"))
sys.exit(0 if all(ok for _, ok, _ in results) else 1)
PY
python3 "$WORK/validate.py" "$WORK" "$START_MS" "$END_MS" || FAILS=$((FAILS + 1))

# ---------------------------------------------------------------------------
echo "== 5. ui.sh against a fake gateway"
mkdir -p "$WORK/gw" "$WORK/ui"
cat > "$WORK/gw/gateway.py" <<'PY'
import hashlib, json, sys, threading
from http.server import ThreadingHTTPServer, BaseHTTPRequestHandler
log = open(sys.argv[1], "a")
store, puts, lock = {}, [0], threading.Lock()
sha = lambda b: hashlib.sha256(b).hexdigest()
etag = lambda b: '"' + hashlib.md5(b).hexdigest() + '"'
norm = lambda e: (e or "").strip().removeprefix("W/").strip('"')
PREFIX = "/lean/v1/ws1/files/"
class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def reply(self, code, body, headers=()):
        data = body if isinstance(body, bytes) else json.dumps(body).encode()
        self.send_response(code)
        for k, v in headers: self.send_header(k, v)
        self.send_header("content-length", str(len(data))); self.end_headers(); self.wfile.write(data)
    def path_of(self):
        if self.headers.get("authorization") != "Bearer tok":
            self.reply(401, {"error": "unauthorized", "message": ""}); return None
        if not self.path.startswith(PREFIX):
            self.reply(404, {"error": "unknown-workspace", "message": ""}); return None
        return self.path[len(PREFIX):]
    def do_GET(self):
        p = self.path_of()
        if p is None: return
        with lock:
            cur = store.get(p)
            if cur is None:
                log.write(json.dumps({"m": "GET", "path": p, "code": 404}) + "\n"); log.flush()
                return self.reply(404, {"error": "no-such-file", "message": p})
            log.write(json.dumps({"m": "GET", "path": p, "code": 200, "sha": sha(cur), "etag": etag(cur)}) + "\n"); log.flush()
            self.reply(200, cur, [("etag", etag(cur))])
    def do_PUT(self):
        p = self.path_of()
        if p is None: return
        body = self.rfile.read(int(self.headers.get("content-length", "0")))
        with lock:
            puts[0] += 1; n = puts[0]
            im, inm = self.headers.get("if-match"), self.headers.get("if-none-match")
            def done(code, out, headers=(), new=None):
                log.write(json.dumps({"m": "PUT", "path": p, "code": code, "sha": sha(body), "if_match": im,
                                      "if_none_match": inm, "etag": new}) + "\n"); log.flush()
                self.reply(code, out, headers)
            if n == 4:
                return done(409, {"error": "barrier-window-open", "message": "window"}, [("retry-after", "1")])
            if n % 5 == 3 and p in store:      # a concurrent writer moved the object
                store[p] = b"concurrent writer " + str(n).encode() + b"\n"
                log.write(json.dumps({"m": "CHAOS", "path": p, "sha": sha(store[p])}) + "\n")
            cur = store.get(p)
            if cur is not None and not im and inm != "*":
                return done(428, {"error": "precondition-required", "message": ""})
            if cur is not None and (inm == "*" or norm(im) != norm(etag(cur))):
                return done(412, {"error": "file-changed", "message": ""}, [("etag", etag(cur))])
            if cur is None and im:
                return done(412, {"error": "file-changed", "message": ""})
            store[p] = body
            done(200, {"etag": etag(body)}, new=etag(body))
srv = ThreadingHTTPServer(("127.0.0.1", 0), H)
print(srv.server_address[1], flush=True)
srv.serve_forever()
PY
python3 "$WORK/gw/gateway.py" "$WORK/gw/log.jsonl" > "$WORK/gw/port" 2> "$WORK/gw/stderr" &
PIDS+=($!)
disown "$!"
for _ in $(seq 1 50); do [ -s "$WORK/gw/port" ] && break; sleep 0.1; done
PORT=$(head -1 "$WORK/gw/port")
ui_rc=0
AGENT_SH="$AGENT_SH" GATEWAY="http://127.0.0.1:$PORT" WORKSPACE=ws1 GATEWAY_TOKEN=tok UI_MODE=hot UI_SEED=5 \
UI_PATHS=3 UI_INTERVAL_MS=50 UI_MAX_WRITES=10 JOURNAL="$WORK/ui/journal.jsonl" CONTROL_DIR="$WORK/ui/ctl" \
    "$SHELL_UNDER_TEST" "$UI_SH" 2> "$WORK/ui/stderr" || ui_rc=$?
check "ui.sh exited 0 after UI_MAX_WRITES (rc=$ui_rc)" test "$ui_rc" = 0
python3 - "$WORK" <<'PY' || FAILS=$((FAILS + 1))
import json, os, sys
work = sys.argv[1]
j = [json.loads(l) for l in open(os.path.join(work, "ui", "journal.jsonl"))]
g = [json.loads(l) for l in open(os.path.join(work, "gw", "log.jsonl"))]
res = []
ops = [l for l in j if l["k"] == "op"]
acks = {l["nonce"]: (i, l) for i, l in enumerate(j) if l["k"] == "ack"}
res.append(("ui: op line before its ack, one ack per op", all(o["nonce"] in acks and acks[o["nonce"]][0] > j.index(o) for o in ops)))
puts = [e for e in g if e["m"] == "PUT"]
res.append(("ui: one PUT per journaled op", len(puts) == len(ops)))
bad = []
last_get = {}
pi = 0
for e in g:
    if e["m"] == "GET":
        last_get = e
    elif e["m"] == "PUT":
        if pi >= len(ops): bad.append("extra PUT"); break
        o, (_, a) = ops[pi], acks[ops[pi]["nonce"]]; pi += 1
        want_base = last_get.get("sha") if last_get.get("code") == 200 else "absent"
        if o["path"] != e["path"] or o["sha256"] != e["sha"] or o["base"] != want_base: bad.append(("op", o, e, last_get))
        want_cond = ("If-Match", last_get.get("etag")) if want_base != "absent" else ("If-None-Match", "*")
        got_cond = ("If-Match", e["if_match"]) if e["if_match"] else ("If-None-Match", e["if_none_match"])
        if got_cond != want_cond: bad.append(("precondition", got_cond, want_cond))
        if e["code"] == 200 and not (a["status"] == "ok" and a.get("etag") == e["etag"]): bad.append(("ok ack", a, e))
        if e["code"] in (409, 412) and not (a["status"] == "refused" and a.get("http") == e["code"]): bad.append(("refused ack", a, e))
res.append((f"ui: base = the GET's bytes, precondition = its etag, ack = the PUT's answer  {bad[:2] if bad else ''}", not bad))
st = [a["status"] for _, a in acks.values()]
res.append((f"ui: exercised create ({sum(o['base'] == 'absent' for o in ops)}), overwrite ({sum(o['base'] != 'absent' for o in ops)}), 412, 409 retry",
            any(o["base"] == "absent" for o in ops) and any(o["base"] != "absent" for o in ops)
            and any(e["code"] == 412 for e in puts) and any(e["code"] == 409 for e in puts) and "ok" in st))
res.append(("ui: journal ends stopped", j[-1]["k"] == "stopped"))
for name, ok in res:
    print(("  PASS  " if ok else "  FAIL  ") + name)
sys.exit(0 if all(ok for _, ok in res) else 1)
PY

echo
if [ "$FAILS" -eq 0 ]; then echo "agent localtest: PASS"; else echo "agent localtest: FAIL ($FAILS)"; fi
[ "$FAILS" -eq 0 ]
