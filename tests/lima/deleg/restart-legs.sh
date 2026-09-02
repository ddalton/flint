#!/usr/bin/env bash
#
# Design §9's restart legs (a) and (c), on the wire.
#
# (a) SAME-PVC RESTART WITH GRANTS OUTSTANDING. This is the [V2-fatal]
#     hole §6 exists for. On a same-PVC restart the client record comes
#     back from the backend with reclaim_complete=true, EXCHANGE_ID hits
#     case 1, and Linux treats it as session loss rather than a server
#     reboot: no CLAIM_PREVIOUS, no cache invalidation. A delegation
#     holder would then serve its page cache forever against a server
#     that has forgotten the delegation — and by design it sends no RPC
#     that could surface BAD_STATEID. The holder-evidence marker is what
#     closes that, and this leg is what proves the marker survives a
#     restart and reaches the client.
#
#     Scored on the CLIENT-VISIBLE signal, not just the server log: the
#     holder's first SEQUENCE after the restart must carry
#     SEQ4_STATUS_RECALLABLE_STATE_REVOKED, and TEST_STATEID on the old
#     delegation stateid must no longer say OK.
#
#     THE CONTROL IS NOT OPTIONAL. "The bit is set" proves nothing on
#     its own — a server that raised it for every client would pass. So
#     a second, DIFFERENT client connects across the same restart and
#     must NOT see the bit.
#
# (c) NO GRANTS DURING GRACE. Immediately after the restart the server
#     is in its grace window and must refuse to hand out delegations;
#     once grace ends it must resume. Both halves are asserted, because
#     "refused during grace" is also what a permanently broken server
#     looks like.
#
# The server runs INSIDE the VM, as root, on ext4, in the flintns
# netns — the same posture the nfstest legs use.
#
# Usage:  tests/lima/deleg/restart-legs.sh [outdir]
set -uo pipefail

REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
BIN="${RESTART_BIN:-$REPO/spdk-csi-driver/target/aarch64-unknown-linux-musl/release/flint-nfs-server}"
OUT="${1:-/tmp/flint-deleg-restart}"
VM="${LIMA_VM:-flint-nfs-client}"
PORT="${RESTART_PORT:-20494}"
EXPORT="/var/tmp/flint-restart-export"
SRV=10.200.0.2
GRACE=20

mkdir -p "$OUT"
fail() { echo "✗ $*" >&2; exit 1; }
note() { echo "  · $*"; }

[ -f "$BIN" ] || fail "missing $BIN (cross-build for aarch64-unknown-linux-musl)"
limactl list "$VM" 2>/dev/null | grep -q Running || fail "VM $VM is not running"

limactl cp "$REPO/tests/lima/deleg/nfstest-3way-setup.sh" "$VM:/tmp/nfstest-3way-setup.sh" >/dev/null
limactl shell "$VM" -- sudo bash /tmp/nfstest-3way-setup.sh > "$OUT/topology.log" 2>&1 \
  || { cat "$OUT/topology.log"; fail "topology setup failed"; }
limactl cp "$REPO/tests/lima/deleg/restart-probe.py" "$VM:/tmp/restart-probe.py" >/dev/null
note "topology up; server will be $SRV:$PORT"

start_server() {   # $1 = label, $2 = fresh|keep
  local label="$1" mode="$2"
  limactl shell "$VM" -- sudo sh -c "ip netns exec flintns pkill -f 'flint-nfs-server --bind-addr 0.0.0.0 --port $PORT' 2>/dev/null; exit 0"
  sleep 2
  if [ "$mode" = fresh ]; then
    limactl shell "$VM" -- sudo sh -c "rm -rf $EXPORT; mkdir -p $EXPORT/.flint-nfs $EXPORT/tmp; printf restartvol > $EXPORT/.flint-nfs/volume-id; chmod 0777 $EXPORT $EXPORT/tmp" \
      || fail "could not prepare the export"
  fi
  limactl shell "$VM" -- sudo sh -c "FLINT_NFS_DELEGATIONS=1 FLINT_NFS_DELEG_REPORT_SECS=5 FLINT_NFS_GRACE_SECS=$GRACE nohup setsid ip netns exec flintns $BIN --bind-addr 0.0.0.0 --port $PORT --export-path $EXPORT --volume-id restartvol </dev/null >/var/tmp/flint-restart-$label.log 2>&1 & echo started" >/dev/null
  for _ in $(seq 1 30); do
    limactl shell "$VM" -- sudo sh -c "ip netns exec flintns ss -ltn | grep -q ':$PORT '" && break
    sleep 1
  done
  limactl shell "$VM" -- sudo sh -c "ip netns exec flintns ss -ltn | grep -q ':$PORT '" \
    || fail "$label: server did not come up"
  limactl shell "$VM" -- sudo grep -q "delegations are ON" "/var/tmp/flint-restart-$label.log" \
    || fail "$label: server does not report delegations ON"
}

probe() {          # $1 = phase, $2.. = extra args
  local phase="$1"; shift
  limactl shell "$VM" -- sudo sh -c "cd /tmp && ip netns exec flintcli2 python3 /tmp/restart-probe.py $phase $SRV $PORT /tmp deleg-restart-file $*" 2>/dev/null | tail -1
}

echo "▶ incarnation 1 — take a delegation and hold it"
start_server one fresh
sleep "$GRACE"     # let grace end so the grant is allowed
HOLD=$(probe hold)
echo "$HOLD" > "$OUT/hold.json"
note "hold: $HOLD"
echo "$HOLD" | grep -q '"deleg_type": 1' \
  || fail "PRECONDITION: no READ delegation was granted, so the restart has nothing to forget: $HOLD"
STATEID=$(python3 -c "import json,sys; print(json.dumps(json.load(open('$OUT/hold.json'))['deleg_stateid']))")

echo "▶ restart on the SAME export (same-PVC)"
start_server two keep
limactl shell "$VM" -- sudo cp "/var/tmp/flint-restart-two.log" /tmp/restart-two.log 2>/dev/null
limactl cp "$VM:/tmp/restart-two.log" "$OUT/server-two.log" >/dev/null 2>&1 || true

# Leg (a) goes FIRST. The bit is delivered on the holder's first
# SEQUENCE and lowered as soon as a later one acks it, so anything that
# establishes a session as the holder beforehand consumes the evidence.
echo "▶ leg (a) — the holder's first SEQUENCE after the restart"
PROBE=$(probe probe "'$STATEID'")
echo "$PROBE" > "$OUT/probe.json"
note "holder probe: $PROBE"

# A DIFFERENT identity, so it cannot disturb the holder's marker.
echo "▶ leg (c) — no grants during grace"
GRACE_HOLD=$(probe gracehold)
echo "$GRACE_HOLD" > "$OUT/grace-hold.json"
note "during grace: $GRACE_HOLD"

echo "▶ control — a client that never held anything, same restart"
FRESH=$(probe fresh)
echo "$FRESH" > "$OUT/fresh.json"
note "fresh client: $FRESH"

echo "▶ after grace — grants resume"
sleep "$GRACE"
AFTER=$(probe gracehold)
echo "$AFTER" > "$OUT/after-grace.json"
note "after grace: $AFTER"

python3 - "$OUT" <<'PY'
import json, os, sys
out = sys.argv[1]
def load(n):
    try:
        return json.load(open(os.path.join(out, n)))
    except Exception as e:
        return {"_error": str(e)}

hold, probe, fresh = load("hold.json"), load("probe.json"), load("fresh.json")
grace, after = load("grace-hold.json"), load("after-grace.json")
bad = []

print()
# LEG (a): the holder must be told.
if not probe.get("recallable_state_revoked"):
    bad.append("leg (a): the holder's first SEQUENCE after the restart did NOT carry "
               f"SEQ4_STATUS_RECALLABLE_STATE_REVOKED (flags={probe.get('seq_status_flags')}). "
               "It would go on serving its page cache against a server that forgot the "
               "delegation, and send no RPC that could reveal it.")
else:
    print("  ✓ leg (a): holder's first SEQUENCE carries RECALLABLE_STATE_REVOKED")

# and the stateid must not still validate
code = probe.get("test_stateid_code")
if code is None:
    bad.append(f"leg (a): TEST_STATEID did not return a per-stateid code: {probe}")
elif code == 0:
    bad.append("leg (a): TEST_STATEID still reports the pre-restart delegation stateid "
               "as VALID — the server forgot the delegation but still vouches for it")
else:
    print(f"  ✓ leg (a): TEST_STATEID rejects the pre-restart stateid (code {code})")

# THE CONTROL: a client that held nothing must not see the bit.
if fresh.get("recallable_state_revoked"):
    bad.append("CONTROL: a client that never held a delegation ALSO got "
               "RECALLABLE_STATE_REVOKED — the bit is not evidence of anything")
else:
    print("  ✓ control: a client that held nothing does NOT get the bit")

# LEG (c): grace refuses, and post-grace resumes.
if grace.get("deleg_type") == 1:
    bad.append("leg (c): a delegation was GRANTED during the grace window")
else:
    print(f"  ✓ leg (c): no delegation granted during grace (type={grace.get('deleg_type')})")
if after.get("deleg_type") != 1:
    bad.append("leg (c): after grace ended, delegations did NOT resume "
               f"(type={after.get('deleg_type')}) — 'refused during grace' is also what a "
               "permanently broken server looks like, so this half is what makes the "
               "other half mean something")
else:
    print("  ✓ leg (c): grants resume once grace ends")

srv = os.path.join(out, "server-two.log")
if os.path.exists(srv):
    txt = open(srv, errors="replace").read()
    if "held recallable state across the restart" in txt:
        print("  ✓ server logged the pre-arm on load")
    else:
        bad.append("the restarted server did not log 'held recallable state across the "
                   "restart' — the marker did not survive, so any bit the client saw "
                   "came from somewhere else")

if bad:
    print("\n✗ FAILED")
    for b in bad: print(f"  ✗ {b}")
    sys.exit(1)
print("\n✓ restart legs (a) and (c) PASS")
PY
