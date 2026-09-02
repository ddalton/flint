#!/usr/bin/env bash
#
# Design §9's negative delegation legs, on the wire.
#
# "Negative legs find the defects" is the GSS lesson, and the three
# protocol defects the first pynfs run turned up (the CREATE arm never
# answering WANT bits, BACKCHANNEL_CTL truncating any compound holding
# it, a revoked delegation answering BAD_STATEID) were all of this
# shape: reachable only by asking the server to REFUSE something.
#
# The tests live in tests/lima/deleg/st_flintdeleg.py and run inside
# pynfs's own harness, so they get sessions, credentials and compound
# plumbing for free. This script installs that module into the VM's
# pynfs tree, runs it against both arms of the flag, and checks the
# outcomes against a table.
#
# WHY BOTH ARMS. Two of these legs (FLINTNEG2/3) are about compound
# SHAPE and must pass whether or not delegations are enabled. That is
# what makes the other two meaningful: if the shape legs pass on the
# OFF arm, the rig demonstrably ran there, so FLINTNEG1/4 failing on
# that arm is a refusal the server made — not a rig that never
# connected. A rig that only ran the ON arm could not tell those apart.
#
# Usage:  tests/lima/deleg/negative-legs.sh [outdir]
set -uo pipefail

REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
BIN="$REPO/spdk-csi-driver/target/release/flint-nfs-server"
MOD="$REPO/tests/lima/deleg/st_flintdeleg.py"
MOD2="$REPO/tests/lima/deleg/st_flintconf.py"
OUT="${1:-/tmp/flint-deleg-neg}"
VM="${LIMA_VM:-flint-nfs-client}"
PORT="${DELEG_NEG_PORT:-20499}"
EXPORT="${DELEG_NEG_EXPORT:-/tmp/flint-deleg-neg-export}"
VOL="negvol"
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
codes = sorted(c for c in cdict if c.startswith(\"FLINTNEG\"))
print(\"discovered:\", \" \".join(codes))
assert len(codes) >= 6, codes
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

  limactl shell "$VM" -- sudo rm -f /tmp/pynfs-neg.json
  limactl shell "$VM" -- bash -lc "
      cd /opt/pynfs/nfs4.1 && \
      timeout 600 python3 ./testserver.py ${HOST}:${PORT}/tmp \
        --maketree --nocleanup --json=/tmp/pynfs-neg.json flintneg" \
    > "$OUT/pynfs-$arm.log" 2>&1
  limactl cp "$VM:/tmp/pynfs-neg.json" "$OUT/neg-$arm.json" >/dev/null 2>&1 \
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
    with open(os.path.join(out, f"neg-{arm}.json")) as f:
        doc = json.load(f)
    res = {}
    for tc in doc.get("testcase", []):
        code = tc.get("code") or ""
        if not code.startswith("FLINTNEG"):
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

if len(codes) < 6:
    bad.append(f"G1: only {len(codes)} flintneg tests ran — the run did not happen")

# The calibration gates the two shape legs. A shape verdict on top of a
# miscalibrated counter is not a finding, it is a rumour.
if off.get("FLINTNEG5", ("-",))[0] != "PASS":
    bad.append("CALIBRATION: FLINTNEG5 did not pass — the compound-shape "
               "verdicts from FLINTNEG2/3 are WITHDRAWN, not reported: "
               f"{off.get('FLINTNEG5', ('-',''))[1][:300]}")

# The table. FLINTNEG2/3 are about compound SHAPE and hold on both
# arms; that they pass on the OFF arm is what proves the rig ran there,
# which is what licenses reading FLINTNEG1/4's failures on that arm as
# the server's refusals rather than as a dead rig.
EXPECT = {
    #            off      on
    "FLINTNEG1": ("FAIL", "PASS"),   # control cannot grant with the flag off
    "FLINTNEG2": ("PASS", "PASS"),   # compound shape, flag-independent
    "FLINTNEG3": ("PASS", "PASS"),   # compound shape, flag-independent
    "FLINTNEG4": ("FAIL", "PASS"),   # NONE_EXT is never sent with the flag off
    # The calibration. If this fails, FLINTNEG2/3 are measuring this
    # client's arithmetic rather than the server, so their verdicts are
    # withdrawn rather than reported.
    "FLINTNEG5": ("PASS", "PASS"),
    # Refusal is flag-independent: flint never supports reclaim.
    "FLINTNEG6": ("PASS", "PASS"),
}
for code, (want_off, want_on) in EXPECT.items():
    got_off = off.get(code, ("-",""))[0]
    got_on  = on.get(code, ("-",""))[0]
    if got_off != want_off:
        bad.append(f"{code}: off arm is {got_off}, expected {want_off}")
    if got_on != want_on:
        bad.append(f"{code}: ON arm is {got_on}, expected {want_on}"
                   f"  ← {on.get(code,('',''))[1][:200]}")

# The liveness precondition: at least one leg must pass on the OFF arm,
# or the arm never ran and every "FAIL" there is meaningless.
if not any(v[0] == "PASS" for v in off.values()):
    bad.append("LIVENESS: nothing passed on the OFF arm — the rig did not run there, "
               "so its failures say nothing about the server")

if bad:
    print("\n✗ FAILED")
    for b in bad: print(f"  ✗ {b}")
    sys.exit(1)
print("\n✓ negative legs PASS on both arms")
PY
