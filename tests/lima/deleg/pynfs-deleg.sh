#!/usr/bin/env bash
#
# The pynfs delegation legs of design §9, run as a PAIR.
#
# pynfs's 10 st_delegation tests are SKIPPED by the `all` flag set, so
# neither committed baseline (171/0/91) says anything about them. This
# runs them explicitly, twice, against one server build: flag OFF
# (control) and flag ON (treatment). The arms differ in exactly one
# dimension — FLINT_NFS_DELEGATIONS — because a comparison between arms
# that differ in more than one thing is not an attribution.
#
# WHY A CONTROL AT ALL, when the treatment is "tests pass": because the
# failure that most resembles success here is a run that did not happen.
# A pynfs invocation that cannot reach the server, or whose flag name
# matched nothing, reports zero failures. The control is what proves the
# rig can see a difference, and G3 below makes an identical pair VOID
# rather than PASS.
#
# Usage:  tests/lima/deleg/pynfs-deleg.sh [outdir]
# Exit:   0 = both arms ran and every guard held. Non-zero = look at it.

set -uo pipefail

REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
BIN="$REPO/spdk-csi-driver/target/release/flint-nfs-server"
OUT="${1:-/tmp/flint-deleg-pynfs}"
VM="${LIMA_VM:-flint-nfs-client}"

# A PRIVATE port and export: two sessions share this VM, and a run that
# quietly attached to another session's server would produce numbers
# about somebody else's build.
PORT="${DELEG_PORT:-20493}"
EXPORT="${DELEG_EXPORT:-/tmp/flint-deleg-export}"
VOL="deleg-vol"
HOST="host.lima.internal"

mkdir -p "$OUT"
fail() { echo "✗ $*" >&2; exit 1; }
note() { echo "  · $*"; }

[ -x "$BIN" ] || fail "missing $BIN (cargo build --release --bin flint-nfs-server)"
command -v limactl >/dev/null || fail "limactl not found"
limactl list "$VM" 2>/dev/null | grep -q Running || fail "VM $VM is not running"

stop_server() {
  if [ -f "$OUT/server.pid" ]; then
    kill "$(cat "$OUT/server.pid")" 2>/dev/null
    # Give it a beat to release the port; a half-dead server on the
    # next arm's port is indistinguishable from a server that refused
    # to start.
    for _ in 1 2 3 4 5 6 7 8 9 10; do
      kill -0 "$(cat "$OUT/server.pid")" 2>/dev/null || break
      sleep 0.3
    done
    kill -9 "$(cat "$OUT/server.pid")" 2>/dev/null
    rm -f "$OUT/server.pid"
  fi
}
trap stop_server EXIT

run_arm() {          # $1 = arm name, $2 = 1|"" for the flag, $3 = extra env
  local arm="$1" flag="$2" extra="${3:-}"
  local log="$OUT/server-$arm.log" json="$OUT/pynfs-$arm.json"

  echo "▶ arm=$arm  FLINT_NFS_DELEGATIONS=${flag:-<unset>}"
  stop_server
  rm -rf "$EXPORT"
  mkdir -p "$EXPORT/.flint-nfs" "$EXPORT/tmp"
  printf '%s' "$VOL" > "$EXPORT/.flint-nfs/volume-id"
  chmod 0777 "$EXPORT/tmp"

  # The reporter's interval is the rig's evidence cadence: at the 60s
  # default a two-minute run can print one line or none.
  env ${flag:+FLINT_NFS_DELEGATIONS=$flag} $extra \
      FLINT_NFS_DELEG_REPORT_SECS=5 \
      FLINT_NFS_GRACE_SECS=900 \
      "$BIN" --bind-addr 0.0.0.0 --port "$PORT" \
             --export-path "$EXPORT" --volume-id "$VOL" \
      > "$log" 2>&1 &
  echo $! > "$OUT/server.pid"
  sleep 3
  kill -0 "$(cat "$OUT/server.pid")" 2>/dev/null \
    || { tail -30 "$log"; fail "$arm: server died on startup"; }

  # G5 — the flag actually reached the server. The reporter announces
  # its own state at startup, so this is the SERVER's word for which
  # arm this is, not the launcher's intent. Without it, a typo in the
  # env name gives two control arms that agree perfectly.
  if [ -n "$flag" ]; then
    grep -q "delegations are OFF" "$log" \
      && fail "G5($arm): asked for delegations ON, server says OFF"
  else
    grep -q "delegations are OFF" "$log" \
      || fail "G5($arm): control arm did not announce delegations OFF"
  fi
  note "G5 ok — server confirms the arm"

  limactl shell "$VM" -- sudo rm -f /tmp/pynfs-deleg.json
  limactl shell "$VM" -- bash -lc "
      cd /opt/pynfs/nfs4.1 && \
      timeout 900 python3 ./testserver.py ${HOST}:${PORT}/tmp \
        --maketree --nocleanup --json=/tmp/pynfs-deleg.json deleg" \
    > "$OUT/pynfs-$arm.log" 2>&1
  note "pynfs exited $? (its status is not the oracle; the JSON is)"

  limactl cp "$VM:/tmp/pynfs-deleg.json" "$json" >/dev/null 2>&1 \
    || fail "G1($arm): no results JSON came back — the run did not happen"

  # G4 — the server outlived the arm. Every test after a crash reports
  # as an ordinary failure, which reads as "the feature is broken"
  # rather than "the server is gone".
  kill -0 "$(cat "$OUT/server.pid")" 2>/dev/null \
    || { tail -40 "$log"; fail "G4($arm): server died DURING the run"; }
  note "G4 ok — server alive at the end"

  cp "$log" "$OUT/server-$arm.final.log"
}

run_arm off ""
run_arm on 1
# A THIRD arm, and it is not redundant. pynfs gives a compound 10 DELAY
# retries at 1s and then gives up, so at the production 90s revoke
# deadline no pynfs test can ever WATCH a revocation happen — DELEG8
# exhausts its budget first. This arm shortens the deadline to 5s so the
# revocation path (revoke -> READ answers DELEG_REVOKED -> SEQ4 bit ->
# TEST_STATEID -> FREE_STATEID clears it) is exercised end to end
# against a real client. The `on` arm above stays at the shipped
# default, so the production posture is still what is measured.
run_arm on-short 1 "FLINT_NFS_DELEG_REVOKE_SECS=5"

python3 - "$OUT" <<'PY'
import json, sys, os, re
out = sys.argv[1]

def load(arm):
    with open(os.path.join(out, f"pynfs-{arm}.json")) as f:
        return json.load(f)

def outcomes(doc):
    # The key is "testcase" (singular), and the array holds ALL 262
    # cases with the 252 the flag did not select marked skipped — so
    # this must filter to st_delegation. Reading the wrong key gave an
    # empty dict and "0 testcases", which G1 caught: a rig that cannot
    # parse its own results looks exactly like a suite that passed.
    res = {}
    for tc in doc.get("testcase", []):
        if tc.get("classname") != "st_delegation":
            continue
        code = tc.get("code") or tc.get("name") or "?"
        if "skipped" in tc:   st = "SKIP"
        elif "failure" in tc: st = "FAIL"
        elif "error" in tc:   st = "ERROR"
        else:                 st = "PASS"
        res[code] = st
    return res

off, on, short = outcomes(load("off")), outcomes(load("on")), outcomes(load("on-short"))
codes = sorted(set(off) | set(on) | set(short))

# What each arm is EXPECTED to say. A table beats eyeballing: without
# it this rig prints a grid and calls anything "moved" a success, so a
# test that regressed from PASS to FAIL would still leave G3 green.
#
# DELEG2 is pinned FAIL on purpose — it asks for a WRITE delegation and
# flint grants only READ delegations (design §1, an explicit non-goal).
# DELEG8 is pinned FAIL at the shipped 90s deadline for a CLIENT-side
# reason: pynfs runs out of retry budget, and its slot bookkeeping then
# leaks the slot it acquired for the retry it never made. Both are
# expectations about a known posture, not permission to ignore a red.
EXPECT_ON = {
    "DELEG1": "PASS", "DELEG2": "FAIL", "DELEG3": "PASS", "DELEG4": "PASS",
    "DELEG5": "PASS", "DELEG6": "PASS", "DELEG7": "PASS", "DELEG8": "FAIL",
    "DELEG9": "PASS", "DELEG23": "PASS",
}
EXPECT_SHORT = dict(EXPECT_ON, DELEG8="PASS")

# G1 — the run has to have HAPPENED. pynfs ships 10 st_delegation
# tests; a flag name that matched nothing yields zero testcases and
# zero failures, which is the shape of a perfect pass.
if len(codes) < 8:
    print(f"✗ G1: only {len(codes)} delegation testcases ran — the run did not happen")
    sys.exit(1)
print(f"  · G1 ok — {len(codes)} delegation testcases in both arms")

# G2 — the ON arm actually GRANTED something. Without this, "DELEG1
# passed" can mean the test never got far enough to need a delegation.
# Count the STATE-LAYER grant line specifically. `try_grant` and the
# OPEN handler both used to log "granted READ delegation" at INFO, so
# counting the phrase read DOUBLE — which does not just inflate a
# number, it silently halves whatever coverage floor a caller thought
# it was enforcing. The OPEN-side line is debug now; the prefix here
# pins which one this counts either way.
#
# Count the per-grant INFO line, not the reporter's delta line. The
# reporter ticks every FLINT_NFS_DELEG_REPORT_SECS and prints nothing
# in between, so a run shorter than one interval emits zero lines —
# and reading that as "zero grants" would condemn a working server.
# The grant line is written at the moment of the grant.
granted = 0
with open(os.path.join(out, "server-on.final.log")) as f:
    for line in f:
        if "deleg: granted READ delegation" in line:
            granted += 1
print(f"  · grants observed on the ON arm: {granted}")

# The control arm must have granted NOTHING. Without this the ON arm's
# count could come from a stale server on the port rather than the one
# this rig started.
off_granted = 0
with open(os.path.join(out, "server-off.final.log")) as f:
    for line in f:
        if "deleg: granted READ delegation" in line:
            off_granted += 1
if off_granted != 0:
    print(f"✗ G6: the CONTROL arm granted {off_granted} delegations — the flag is not what separates the arms")
    sys.exit(1)
print("  · G6 ok — the control arm granted nothing")

print()
print(f"{'CODE':<10} {'OFF':<8} {'ON(90s)':<9} {'ON(5s)':<8}  verdict")
moved = 0
drift = []
for c in codes:
    a, b, d = off.get(c, "-"), on.get(c, "-"), short.get(c, "-")
    if a != b: moved += 1
    bad = []
    if b != EXPECT_ON.get(c): bad.append(f"ON expected {EXPECT_ON.get(c)}")
    if d != EXPECT_SHORT.get(c): bad.append(f"ON-5s expected {EXPECT_SHORT.get(c)}")
    verdict = "ok" if not bad else "  <-- " + "; ".join(bad)
    if bad: drift.append(c)
    print(f"{c:<10} {a:<8} {b:<9} {d:<8}  {verdict}")
print()
if drift:
    print(f"✗ G7: {len(drift)} testcode(s) do not match the recorded expectation: {drift}")
    sys.exit(1)
print("  · G7 ok — every testcode matches its recorded expectation")

# G3 — the liveness precondition, in its sharpest form. If the flag
# changed nothing, the two arms agree perfectly and every "PASS" above
# is a statement about the control. That is VOID, not PASS.
if moved == 0:
    print("✗ G3: the arms are IDENTICAL — the flag did nothing the suite can see. VOID, not PASS.")
    sys.exit(1)
print(f"  · G3 ok — {moved} testcode(s) moved between arms")

if granted == 0:
    print("✗ G2: the ON arm granted ZERO delegations — every result above is about a server that never delegated.")
    sys.exit(1)
print("  · G2 ok — the ON arm really did grant")
PY
rc=$?
echo
[ $rc -eq 0 ] && echo "✓ both arms ran; guards held" || echo "✗ guards failed (rc=$rc)"
exit $rc
