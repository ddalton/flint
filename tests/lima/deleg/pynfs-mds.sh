#!/usr/bin/env bash
#
# Design §9, the MDS arm: the delegation legs run against the pNFS MDS
# binary, in real MDS posture, over the wire.
#
# WHY A SEPARATE RIG FROM pynfs-deleg.sh. That rig runs
# flint-nfs-server, which has no pNFS handler and therefore no MDS
# posture. Slice 5's whole subject — the second flag
# (FLINT_NFS_DELEGATIONS_PNFS), grant rule 6, and the layout probe that
# fails CLOSED without one — is unreachable from it. Every unit test
# for that code constructs the posture by hand; nothing until now had
# asked a real client to open a file against a real MDS.
#
# THE THREE ARMS, and why the middle one is the point:
#
#   off        FLINT_NFS_DELEGATIONS unset          → zero grants
#   pnfs-off   FLINT_NFS_DELEGATIONS=1 only         → zero grants
#   on         both flags                           → grants, tests pass
#
# The `pnfs-off` arm is what makes this rig worth running. It is the
# only arm that can distinguish "the MDS posture gate works" from "the
# MDS cannot grant delegations at all" — two states that produce
# identical output (zero grants) and would both be reported as success
# by a rig that only ran `off` and `on`. If `pnfs-off` and `on` agree,
# something is broken no matter which way they agree, and G-GATE below
# makes that VOID rather than PASS.
#
# Usage:  tests/lima/deleg/pynfs-mds.sh [outdir]
# Exit:   0 = every arm ran and every guard held.

set -uo pipefail

REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
BIN="$REPO/spdk-csi-driver/target/release/flint-pnfs-mds"
OUT="${1:-/tmp/flint-deleg-mds}"
VM="${LIMA_VM:-flint-nfs-client}"

# PRIVATE port and export: two sessions share this VM, and a run that
# quietly attached to another session's MDS would report numbers about
# somebody else's build. 20490 is bench.sh's; this is not.
PORT="${DELEG_MDS_PORT:-20496}"
GRPC_PORT="${DELEG_MDS_GRPC:-50251}"
EXPORT="${DELEG_MDS_EXPORT:-/tmp/flint-deleg-mds-export}"
HOST="host.lima.internal"

mkdir -p "$OUT"
fail() { echo "✗ $*" >&2; exit 1; }
note() { echo "  · $*"; }

[ -x "$BIN" ] || fail "missing $BIN (cargo build --release --bin flint-pnfs-mds)"
command -v limactl >/dev/null || fail "limactl not found"
limactl list "$VM" 2>/dev/null | grep -q Running || fail "VM $VM is not running"

stop_server() {
  if [ -f "$OUT/server.pid" ]; then
    kill "$(cat "$OUT/server.pid")" 2>/dev/null
    for _ in $(seq 1 20); do
      kill -0 "$(cat "$OUT/server.pid")" 2>/dev/null || break
      sleep 0.3
    done
    kill -9 "$(cat "$OUT/server.pid")" 2>/dev/null
    rm -f "$OUT/server.pid"
  fi
}
trap stop_server EXIT

# A squatted port silently reports ANOTHER server's behaviour. That
# cost a whole DELEG8 investigation once, so it is checked, not hoped.
for p in "$PORT" "$GRPC_PORT"; do
  while lsof -ti :"$p" >/dev/null 2>&1; do
    lsof -ti :"$p" | xargs kill -9 2>/dev/null; sleep 1
  done
done

# The MDS config. `mode: mds` (NOT standalone) is the load-bearing
# line: standalone builds the dispatcher with pnfs_ops = None, which
# means no MDS posture, which means this rig would silently be a second
# copy of pynfs-deleg.sh. G-POSTURE below checks the server agrees.
#
# No DS fleet is configured or started. The delegation legs never call
# LAYOUTGET, and an MDS with zero data servers is still in MDS posture —
# which is the posture under test. Adding DSes would add moving parts
# without adding coverage.
cat > "$OUT/mds.yaml" <<YAML
apiVersion: chert.us/v1alpha1
kind: PnfsConfig
mode: mds
mds:
  bind:
    address: "0.0.0.0"
    port: $PORT
  layout:
    type: file
    stripeSize: 8388608
    policy: stripe
  dataServers: []
  state:
    backend: memory
    config: {}
  ha:
    enabled: false
    replicas: 1
    leaderElection: false
    leaseDuration: 15
    renewDeadline: 10
    retryPeriod: 2
  failover:
    heartbeatTimeout: 30
    policy: recall_affected
    gracePeriod: 60
exports:
  - path: $EXPORT
    fsid: 1
    options: [rw, sync, no_subtree_check]
    access:
      - network: 0.0.0.0/0
        permissions: rw
logging:
  level: info
  format: text
  components:
    mds: info
    layout: info
monitoring:
  prometheus: { enabled: false, port: 0, path: /metrics }
  health: { enabled: false, port: 0, path: /health }
  metrics: []
development:
  debug: false
  simulateFailures: { enabled: false, interval: 0, duration: 0 }
  traceRpc: false
  dumpLayouts: false
YAML

run_arm() {          # $1 = arm, $2 = FLINT_NFS_DELEGATIONS, $3 = FLINT_NFS_DELEGATIONS_PNFS
  local arm="$1" deleg="$2" pnfs="$3"
  local log="$OUT/server-$arm.log" json="$OUT/pynfs-$arm.json"

  echo "▶ arm=$arm  DELEGATIONS=${deleg:-<unset>}  DELEGATIONS_PNFS=${pnfs:-<unset>}"
  stop_server
  rm -rf "$EXPORT"; mkdir -p "$EXPORT/tmp"; chmod 0777 "$EXPORT" "$EXPORT/tmp"

  env ${deleg:+FLINT_NFS_DELEGATIONS=$deleg} \
      ${pnfs:+FLINT_NFS_DELEGATIONS_PNFS=$pnfs} \
      PNFS_MODE=mds \
      FLINT_MDS_GRPC_PORT=$GRPC_PORT \
      FLINT_NFS_DELEG_REPORT_SECS=5 \
      FLINT_NFS_GRACE_SECS=900 \
      "$BIN" --config "$OUT/mds.yaml" > "$log" 2>&1 &
  echo $! > "$OUT/server.pid"

  for _ in $(seq 1 30); do
    grep -qE "pNFS MDS serving NFSv4|Address already" "$log" 2>/dev/null && break
    sleep 1
  done
  grep -q "Address already" "$log" && fail "$arm: port $PORT squatted"
  kill -0 "$(cat "$OUT/server.pid")" 2>/dev/null \
    || { tail -40 "$log"; fail "$arm: MDS died on startup"; }

  # G-POSTURE — the server's OWN word that this is an MDS, not the
  # standalone posture. Without it the whole rig could be measuring a
  # server with no pNFS handler and reporting it as an MDS result.
  grep -q "Posture: STANDALONE" "$log" \
    && fail "G-POSTURE($arm): server came up STANDALONE — mode: mds did not take"

  # G5 — the server's own word for which arm this is. A typo in an env
  # name otherwise yields two control arms that agree perfectly.
  if [ -z "$deleg" ]; then
    grep -q "delegations are OFF" "$log" \
      || fail "G5($arm): control arm did not announce delegations OFF"
  else
    grep -q "delegations are ON" "$log" \
      || { tail -20 "$log"; fail "G5($arm): asked for delegations ON, server did not say so"; }
    grep -q "posture=MDS" "$log" \
      || fail "G-POSTURE($arm): reporter did not confirm MDS posture"
    if [ -z "$pnfs" ]; then
      grep -q "pnfs gate=OFF" "$log" \
        || fail "G-GATE($arm): expected the pNFS gate CLOSED, server says otherwise"
    else
      grep -q "pnfs gate=ON" "$log" \
        || fail "G-GATE($arm): expected the pNFS gate OPEN, server says otherwise"
      # Rule 6's oracle. Absent, the rule fails closed and every grant
      # is refused — which looks exactly like a working gate saying no.
      grep -q "layout probe=installed" "$log" \
        || fail "G-PROBE($arm): the layout probe is missing; rule 6 would refuse everything"
    fi
  fi
  note "G5/G-POSTURE/G-GATE ok — server confirms the arm"

  limactl shell "$VM" -- sudo rm -f /tmp/pynfs-mds-deleg.json
  limactl shell "$VM" -- bash -lc "
      cd /opt/pynfs/nfs4.1 && \
      timeout 900 python3 ./testserver.py ${HOST}:${PORT}/tmp \
        --maketree --nocleanup --json=/tmp/pynfs-mds-deleg.json deleg" \
    > "$OUT/pynfs-$arm.log" 2>&1
  note "pynfs exited $? (its status is not the oracle; the JSON is)"

  limactl cp "$VM:/tmp/pynfs-mds-deleg.json" "$json" >/dev/null 2>&1 \
    || fail "G1($arm): no results JSON came back — the run did not happen"

  kill -0 "$(cat "$OUT/server.pid")" 2>/dev/null \
    || { tail -40 "$log"; fail "G4($arm): MDS died DURING the run"; }
  note "G4 ok — MDS alive at the end"
  cp "$log" "$OUT/server-$arm.final.log"
}

run_arm off      ""  ""
run_arm pnfs-off "1" ""
run_arm on       "1" "1"

python3 - "$OUT" <<'PY'
import json, sys, os
out = sys.argv[1]

def outcomes(arm):
    with open(os.path.join(out, f"pynfs-{arm}.json")) as f:
        doc = json.load(f)
    res = {}
    for tc in doc.get("testcase", []):
        if tc.get("classname") != "st_delegation":
            continue
        code = tc.get("code") or tc.get("name") or "?"
        if   "skipped" in tc: st = "SKIP"
        elif "failure" in tc: st = "FAIL"
        elif "error"   in tc: st = "ERROR"
        else:                 st = "PASS"
        res[code] = st
    return res

def grants(arm):
    n = 0
    with open(os.path.join(out, f"server-{arm}.final.log")) as f:
        for line in f:
            if "deleg: granted READ delegation" in line:
                n += 1
    return n

off, pnfs_off, on = outcomes("off"), outcomes("pnfs-off"), outcomes("on")
g_off, g_pnfs_off, g_on = grants("off"), grants("pnfs-off"), grants("on")
codes = sorted(set(off) | set(pnfs_off) | set(on))
bad = []

print(f"\n  grants: off={g_off}  pnfs-off={g_pnfs_off}  on={g_on}")
print(f"  {'test':<9} {'off':<6} {'pnfs-off':<9} {'on':<6}")
for c in codes:
    print(f"  {c:<9} {off.get(c,'-'):<6} {pnfs_off.get(c,'-'):<9} {on.get(c,'-'):<6}")

# G1 — the run has to have HAPPENED. A flag name matching nothing
# yields zero testcases and zero failures: the shape of a perfect pass.
if len(codes) < 8:
    bad.append(f"G1: only {len(codes)} delegation testcases ran — the run did not happen")
else:
    print(f"\n  · G1 ok — {len(codes)} delegation testcases per arm")

# G6 — the master-flag control granted nothing.
if g_off != 0:
    bad.append(f"G6: control arm granted {g_off} delegations with the flag off")
else:
    print("  · G6 ok — master-flag control granted nothing")

# G-GATE — THE POINT OF THIS RIG. The pNFS gate closed must grant
# nothing, and the gate open must grant something. Either half alone is
# satisfiable by a broken MDS.
if g_pnfs_off != 0:
    bad.append(f"G-GATE: FLINT_NFS_DELEGATIONS_PNFS unset but the MDS still granted {g_pnfs_off}")
elif g_on == 0:
    bad.append("G-GATE: both flags on and the MDS granted NOTHING — "
               "the posture gate is indistinguishable from an MDS that cannot grant at all")
else:
    print(f"  · G-GATE ok — 0 grants with the pNFS gate closed, {g_on} with it open")

# G2 — a grant floor on the ON arm. pynfs's deleg set opens enough
# files that a working MDS grants several; 1 would suggest a single
# lucky path rather than a working feature.
if g_on < 5:
    bad.append(f"G2: only {g_on} grants on the ON arm — below the coverage floor")

# G3 — an identical pair is VOID, not PASS. If the flag changed
# nothing, every PASS is a statement about the control.
if on == pnfs_off:
    bad.append("G3: the ON and pnfs-off arms are identical — the pNFS flag changed nothing")

# The expectations, from the standalone MDS-free run. Same server code
# path for the grant decision, so the same table should hold; anything
# that differs is a finding about the MDS posture and must be looked at
# rather than absorbed.
EXPECT_ON = {
    "DELEG1": "PASS", "DELEG2": "FAIL", "DELEG3": "PASS", "DELEG4": "PASS",
    "DELEG5": "PASS", "DELEG6": "PASS", "DELEG7": "PASS", "DELEG8": "FAIL",
    "DELEG9": "PASS", "DELEG23": "PASS",
}
for code, want in EXPECT_ON.items():
    got = on.get(code)
    if got is None:
        bad.append(f"EXPECT: {code} did not run on the ON arm")
    elif got != want:
        bad.append(f"EXPECT: {code} is {got} on the ON arm, expected {want}")

if bad:
    print("\n✗ FAILED")
    for b in bad: print(f"  ✗ {b}")
    sys.exit(1)
print("\n✓ MDS delegation legs PASS — posture real, gate real, expectations held")
PY
