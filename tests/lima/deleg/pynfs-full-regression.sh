#!/usr/bin/env bash
#
# Design §9: "Re-run the full 262 with the flag ON: floor-171 must not
# regress."
#
# The gate here is PER-TEST, not a pass count. A count can hold steady
# while one test breaks and another starts passing, and that is exactly
# the regression worth catching — the delegation grant path runs inside
# the ordinary OPEN handler, so its blast radius is every test that
# opens a file, not the ten with `deleg` in their flags.
#
# Usage:  tests/lima/deleg/pynfs-full-regression.sh [outdir]
set -uo pipefail

REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
BIN="$REPO/spdk-csi-driver/target/release/flint-nfs-server"
OUT="${1:-/tmp/flint-deleg-full}"
VM="${LIMA_VM:-flint-nfs-client}"
PORT="${DELEG_PORT:-20495}"
EXPORT="${DELEG_EXPORT:-/tmp/flint-deleg-full-export}"
VOL="fullvol"
HOST="host.lima.internal"

mkdir -p "$OUT"
fail() { echo "✗ $*" >&2; exit 1; }
[ -x "$BIN" ] || fail "missing $BIN"

stop() { [ -f "$OUT/pid" ] && { kill "$(cat "$OUT/pid")" 2>/dev/null; rm -f "$OUT/pid"; }; }
trap stop EXIT

run_arm() {
  local arm="$1" flag="$2"
  echo "▶ full suite, arm=$arm"
  stop
  # A squatted port silently reports ANOTHER server's behaviour; this
  # cost a whole DELEG8 investigation once.
  while lsof -ti :"$PORT" >/dev/null 2>&1; do lsof -ti :"$PORT" | xargs kill -9 2>/dev/null; sleep 1; done
  rm -rf "$EXPORT"; mkdir -p "$EXPORT/.flint-nfs" "$EXPORT/tmp"
  printf '%s' "$VOL" > "$EXPORT/.flint-nfs/volume-id"; chmod 0777 "$EXPORT/tmp"
  # pynfs's grace tests assume the server is in grace whenever they run,
  # and the suite outlasts the RFC-default 90s window.
  env ${flag:+FLINT_NFS_DELEGATIONS=$flag} FLINT_NFS_GRACE_SECS=900 \
      FLINT_NFS_DELEG_REPORT_SECS=30 \
      "$BIN" --bind-addr 0.0.0.0 --port "$PORT" \
             --export-path "$EXPORT" --volume-id "$VOL" \
      > "$OUT/server-$arm.log" 2>&1 &
  echo $! > "$OUT/pid"
  until grep -qE "NFSv4.2 server on|Address already" "$OUT/server-$arm.log" 2>/dev/null; do sleep 1; done
  grep -q "Address already" "$OUT/server-$arm.log" && fail "$arm: port $PORT squatted"
  # The server's OWN word for which arm this is.
  if [ -n "$flag" ]; then
    grep -q "delegations are OFF" "$OUT/server-$arm.log" && fail "$arm: asked ON, server says OFF"
  else
    grep -q "delegations are OFF" "$OUT/server-$arm.log" || fail "$arm: control did not announce OFF"
  fi

  limactl shell "$VM" -- sudo rm -f /tmp/pynfs-full.json
  limactl shell "$VM" -- bash -lc "
      cd /opt/pynfs/nfs4.1 && \
      timeout 2400 python3 ./testserver.py ${HOST}:${PORT}/tmp \
        --maketree --nocleanup --json=/tmp/pynfs-full.json all" \
    > "$OUT/pynfs-$arm.log" 2>&1
  limactl cp "$VM:/tmp/pynfs-full.json" "$OUT/pynfs-$arm.json" >/dev/null 2>&1 \
    || fail "$arm: no results JSON — the run did not happen"
  kill -0 "$(cat "$OUT/pid")" 2>/dev/null || fail "$arm: server died DURING the run"
  echo "  · ran, server survived"
}

run_arm off ""
run_arm on 1

python3 - "$OUT" <<'PY'
import json, os, sys
out = sys.argv[1]
def outcomes(arm):
    d = json.load(open(os.path.join(out, f"pynfs-{arm}.json")))
    r = {}
    for tc in d.get("testcase", []):
        code = tc.get("code") or tc.get("name")
        if "skipped" in tc:   st = "SKIP"
        elif "failure" in tc: st = "FAIL"
        elif "error" in tc:   st = "ERROR"
        else:                 st = "PASS"
        r[code] = st
    return r

off, on = outcomes("off"), outcomes("on")
codes = sorted(set(off) | set(on))
# The suite is ~262 cases; far fewer means the run did not really happen,
# which is the failure that most resembles success.
if len(codes) < 200:
    print(f"✗ only {len(codes)} testcases — the run did not happen"); sys.exit(1)

def n(d, st): return sum(1 for v in d.values() if v == st)
print(f"\n           PASS  FAIL  SKIP  ERROR")
for name, d in (("flag OFF", off), ("flag ON ", on)):
    print(f"{name}   {n(d,'PASS'):>4}  {n(d,'FAIL'):>4}  {n(d,'SKIP'):>4}  {n(d,'ERROR'):>5}")

regressions = [c for c in codes if off.get(c) == "PASS" and on.get(c) != "PASS"]
improvements = [c for c in codes if off.get(c) != "PASS" and on.get(c) == "PASS"]
print(f"\nPASS -> not-PASS with the flag on: {regressions or 'none'}")
print(f"not-PASS -> PASS with the flag on: {improvements or 'none'}")

if n(off, "PASS") < 171:
    print(f"✗ the CONTROL arm is below the recorded floor of 171 ({n(off,'PASS')}) — "
          f"this run says nothing about delegations")
    sys.exit(1)
if regressions:
    print(f"✗ {len(regressions)} test(s) regressed with the flag on")
    sys.exit(1)
print("\n✓ no per-test regression; control clears the floor")
PY
