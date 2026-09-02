#!/usr/bin/env bash
#
# Design §9's conflict-site matrix, on the wire, two clients.
#
# For every mutating site §5.2 routes through the fence, client A holds
# a READ delegation and client B performs the mutation. The required
# sequence is: B's FIRST attempt is DELAYed, A observes CB_RECALL and
# returns, B's retry succeeds.
#
# The first of those is the assertion that matters. pynfs's own DELEG1
# accepts either OK or DELAY from the conflicting open, which passes
# equally against a server that never fenced anything — silent success
# is exactly the failure this matrix exists to catch. The tests in
# st_flintconf.py require DELAY.
#
# The OFF arm is expected to FAIL every leg: each one begins by
# requiring client A to actually hold a delegation, which cannot happen
# with the feature switched off. That asymmetry is the control — if the
# legs passed on both arms they would not be measuring the fence.
#
# Usage:  tests/lima/deleg/conflict-matrix.sh [outdir]
set -uo pipefail

REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
BIN="$REPO/spdk-csi-driver/target/release/flint-nfs-server"
MOD="$REPO/tests/lima/deleg/st_flintdeleg.py"
MOD2="$REPO/tests/lima/deleg/st_flintconf.py"
OUT="${1:-/tmp/flint-deleg-conf}"
VM="${LIMA_VM:-flint-nfs-client}"
PORT="${DELEG_CONF_PORT:-20498}"
EXPORT="${DELEG_NEG_EXPORT:-/tmp/flint-deleg-conf-export}"
VOL="confvol"
HOST="host.lima.internal"

mkdir -p "$OUT"
fail() { echo "✗ $*" >&2; exit 1; }
note() { echo "  · $*"; }

[ -x "$BIN" ] || fail "missing $BIN (cargo build --release --bin flint-nfs-server)"
[ -f "$MOD" ] || fail "missing $MOD"
[ -f "$MOD2" ] || fail "missing $MOD2"
limactl list "$VM" 2>/dev/null | grep -q Running || fail "VM $VM is not running"

stop() {
  if [ -f "$OUT/pid" ]; then
    kill "$(cat "$OUT/pid")" 2>/dev/null
    for _ in $(seq 1 20); do kill -0 "$(cat "$OUT/pid")" 2>/dev/null || break; sleep 0.3; done
    kill -9 "$(cat "$OUT/pid")" 2>/dev/null; rm -f "$OUT/pid"
  fi
}
trap stop EXIT

# ── install the test module into the VM's pynfs ──────────────────────
# pynfs discovers tests from server41tests/__init__.py's __all__, not
# by globbing the directory: copying the file alone installs nothing
# and the run reports zero tests, which reads exactly like a pass.
limactl cp "$MOD" "$VM:/tmp/st_flintdeleg.py" >/dev/null \
  || fail "could not copy the test module into the VM"
limactl cp "$MOD2" "$VM:/tmp/st_flintconf.py" >/dev/null \
  || fail "could not copy the conflict-matrix module into the VM"
limactl cp "$REPO/tests/lima/deleg/register-flintdeleg.py" \
           "$VM:/tmp/register-flintdeleg.py" >/dev/null \
  || fail "could not copy the registration script into the VM"
limactl shell "$VM" -- sudo bash -c '
  set -e
  cp /tmp/st_flintdeleg.py /opt/pynfs/nfs4.1/server41tests/st_flintdeleg.py
  cp /tmp/st_flintconf.py   /opt/pynfs/nfs4.1/server41tests/st_flintconf.py
  python3 /tmp/register-flintdeleg.py st_flintdeleg.py st_flintconf.py
' > "$OUT/install.log" 2>&1 || { cat "$OUT/install.log"; fail "module install failed"; }
note "module installed: $(tail -1 "$OUT/install.log")"

# It must be IMPORTABLE and its tests DISCOVERABLE. A module that
# raises on import is silently skipped by some harnesses; here it would
# take the whole run down, which is better — but check anyway, because
# "0 tests ran" is the failure that looks most like success.
limactl shell "$VM" -- bash -lc '
  cd /opt/pynfs/nfs4.1 && PYTHONPATH=/opt/pynfs python3 -c "
import testmod
tests, fdict, cdict = testmod.createtests(\"server41tests\")
codes = sorted(c for c in cdict if c.startswith(\"FLINTCONF\"))
print(\"discovered:\", \" \".join(codes))
assert len(codes) >= 5, codes
"' > "$OUT/discover.log" 2>&1 || { cat "$OUT/discover.log"; fail "the tests are not discoverable"; }
note "$(grep discovered "$OUT/discover.log")"

run_arm() {
  local arm="$1" flag="$2"
  echo "▶ arm=$arm  FLINT_NFS_DELEGATIONS=${flag:-<unset>}"
  stop
  while lsof -ti :"$PORT" >/dev/null 2>&1; do lsof -ti :"$PORT" | xargs kill -9 2>/dev/null; sleep 1; done
  rm -rf "$EXPORT"; mkdir -p "$EXPORT/.flint-nfs" "$EXPORT/tmp"
  printf '%s' "$VOL" > "$EXPORT/.flint-nfs/volume-id"; chmod 0777 "$EXPORT/tmp"

  env ${flag:+FLINT_NFS_DELEGATIONS=$flag} \
      FLINT_NFS_DELEG_REPORT_SECS=5 FLINT_NFS_GRACE_SECS=900 \
      "$BIN" --bind-addr 0.0.0.0 --port "$PORT" \
             --export-path "$EXPORT" --volume-id "$VOL" \
      > "$OUT/server-$arm.log" 2>&1 &
  echo $! > "$OUT/pid"
  for _ in $(seq 1 30); do
    grep -qE "NFSv4.2 server on|Address already" "$OUT/server-$arm.log" 2>/dev/null && break
    sleep 1
  done
  grep -q "Address already" "$OUT/server-$arm.log" && fail "$arm: port $PORT squatted"

  # The server's own word for the arm.
  if [ -n "$flag" ]; then
    grep -q "delegations are ON" "$OUT/server-$arm.log" \
      || fail "G5($arm): asked for delegations ON, server did not say so"
  else
    grep -q "delegations are OFF" "$OUT/server-$arm.log" \
      || fail "G5($arm): control arm did not announce delegations OFF"
  fi

  limactl shell "$VM" -- sudo rm -f /tmp/pynfs-conf.json
  limactl shell "$VM" -- bash -lc "
      cd /opt/pynfs/nfs4.1 && \
      timeout 600 python3 ./testserver.py ${HOST}:${PORT}/tmp \
        --maketree --nocleanup --json=/tmp/pynfs-conf.json flintconf" \
    > "$OUT/pynfs-$arm.log" 2>&1
  limactl cp "$VM:/tmp/pynfs-conf.json" "$OUT/conf-$arm.json" >/dev/null 2>&1 \
    || fail "G1($arm): no results JSON — the run did not happen"
  kill -0 "$(cat "$OUT/pid")" 2>/dev/null \
    || { tail -30 "$OUT/server-$arm.log"; fail "G4($arm): server died DURING the run"; }
  cp "$OUT/server-$arm.log" "$OUT/server-$arm.final.log"
  note "arm complete"
}

run_arm off ""
run_arm on 1

python3 - "$OUT" <<'PY'
import json, os, sys
out = sys.argv[1]

def outcomes(arm):
    with open(os.path.join(out, f"conf-{arm}.json")) as f:
        doc = json.load(f)
    res = {}
    for tc in doc.get("testcase", []):
        code = tc.get("code") or ""
        if not code.startswith("FLINTCONF"):
            continue
        if   "skipped" in tc: st = "SKIP"
        elif "failure" in tc: st = "FAIL"
        elif "error"   in tc: st = "ERROR"
        else:                 st = "PASS"
        res[code] = (st, (tc.get("failure") or tc.get("error") or {}).get("message", "")
                     if isinstance(tc.get("failure") or tc.get("error"), dict) else "")
    return res

off, on = outcomes("off"), outcomes("on")
codes = sorted(set(off) | set(on))
bad = []

print(f"\n  {'code':<12} {'off':<7} {'on':<7}")
for c in codes:
    print(f"  {c:<12} {off.get(c,('-',''))[0]:<7} {on.get(c,('-',''))[0]:<7}")

if len(codes) < 5:
    bad.append(f"G1: only {len(codes)} flintconf tests ran — the run did not happen")

EXPECT = {
    #              off      on
    "FLINTCONF1": ("FAIL", "PASS"),   # open_write
    "FLINTCONF2": ("FAIL", "PASS"),   # remove
    "FLINTCONF3": ("FAIL", "PASS"),   # rename_src
    "FLINTCONF4": ("FAIL", "PASS"),   # link  (the hardlink-alias site)
    "FLINTCONF5": ("FAIL", "PASS"),   # setattr
}
for code, (want_off, want_on) in EXPECT.items():
    got_off = off.get(code, ("-",""))[0]
    got_on  = on.get(code, ("-",""))[0]
    if got_off != want_off:
        bad.append(f"{code}: off arm is {got_off}, expected {want_off}")
    if got_on != want_on:
        bad.append(f"{code}: ON arm is {got_on}, expected {want_on}"
                   f"  ← {on.get(code,('',''))[1][:200]}")

# LIVENESS, stated differently here. Every leg is expected to fail on
# the OFF arm, so "nothing passed" cannot distinguish a working control
# from a rig that never connected. What can: the arm has to have RUN the
# same set of tests. If the OFF arm reports fewer legs than the ON arm,
# it did not run them and its failures are silence, not refusals.
if set(off) != set(on):
    bad.append(f"LIVENESS: the arms ran different tests (off={sorted(off)}, "
               f"on={sorted(on)}) — the OFF arm's failures say nothing")

if bad:
    print("\n✗ FAILED")
    for b in bad: print(f"  ✗ {b}")
    sys.exit(1)
print("\n✓ conflict-site matrix PASS — every site DELAYed B, recalled A, then let B through")
PY
