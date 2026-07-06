#!/usr/bin/env bash
#
# identity-drill.sh — durable-DS plan Phase 2 gate: the DS identity ↔
# data-volume binding guard.
#
# What it asserts
#   1. First boot stamps `.flint-ds-identity` (device_id + creation
#      stamp) in the data dir.
#   2. A restart of the SAME device on the same dir verifies the marker
#      and serves (stamp unchanged — not re-stamped).
#   3. A DIFFERENT device pointed at that dir (the DS-B-volume-into-
#      DS-A's-pod scenario: re-pointed PVC, wrong restore, identity
#      aliasing) REFUSES to start, before serving a single byte.
#
# Pure-host drill: the refusal fires in DataServer::new, before any
# MDS contact, so no MDS/VM is needed. Exit 0 = PASS.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
BIN="$REPO_ROOT/spdk-csi-driver/target/release/flint-pnfs-ds"
WORK=$(mktemp -d /tmp/flint-identity-drill.XXXXXX)
DATA_DIR="$WORK/data"
LOG_DIR="$WORK"

cleanup() {
  set +e
  pkill -f "flint-pnfs-ds.*identity-drill" 2>/dev/null
  [ -n "${KEEP:-}" ] || rm -rf "$WORK"
}
trap cleanup EXIT

step() { printf '\n▶ %s\n' "$*"; }
ok()   { printf '✓ %s\n' "$*"; }
fail() { printf '\n✗ %s\n' "$*" >&2; exit 1; }

[ -x "$BIN" ] || fail "missing $BIN (cd spdk-csi-driver && cargo build --release --bin flint-pnfs-ds)"
mkdir -p "$DATA_DIR"

# Minimal DS config; the MDS endpoint points at a dead port — the
# guard fires before registration, and registration failures don't
# block startup anyway.
mkconfig() { # <device_id> <port> <out>
  cat > "$3" <<EOF
apiVersion: flint.io/v1alpha1
kind: PnfsConfig
mode: ds
ds:
  bind: { address: "127.0.0.1", port: $2 }
  deviceId: $1
  mds:
    endpoint: "127.0.0.1:1"   # dead on purpose — guard fires first
    heartbeatInterval: 60
    registrationRetry: 5
    maxRetries: 0
  bdevs:
    - name: lvol0
      mount_point: $DATA_DIR
      spdk_volume: lvol0
exports:
  - path: /
    fsid: 1
    options: [rw, sync]
    access:
      - network: 0.0.0.0/0
        permissions: rw
logging: { level: info, format: text }
EOF
}

wait_for() { # <timeout_s> <cmd...>
  local t=$1; shift
  local i=0
  until "$@" 2>/dev/null; do
    i=$((i+1)); [ "$i" -ge $((t*10)) ] && return 1
    sleep 0.1
  done
}

MARKER="$DATA_DIR/.flint-ds-identity"

step "first boot as ds-alpha stamps the marker"
mkconfig ds-alpha 29491 "$WORK/ds-alpha.yaml"
PNFS_MODE=ds "$BIN" --config "$WORK/ds-alpha.yaml" > "$LOG_DIR/alpha1.log" 2>&1 &
A_PID=$!
wait_for 10 test -f "$MARKER" || { cat "$LOG_DIR/alpha1.log" >&2; fail "marker never appeared"; }
grep -q "device_id=ds-alpha" "$MARKER" || fail "marker does not carry ds-alpha: $(cat "$MARKER")"
STAMP1=$(grep created_at= "$MARKER")
ok "marker stamped for ds-alpha ($STAMP1)"
kill "$A_PID" 2>/dev/null; wait "$A_PID" 2>/dev/null

step "restart as ds-alpha verifies (stamp unchanged)"
PNFS_MODE=ds "$BIN" --config "$WORK/ds-alpha.yaml" > "$LOG_DIR/alpha2.log" 2>&1 &
A_PID=$!
wait_for 10 grep -q "Identity marker verified" "$LOG_DIR/alpha2.log" \
  || { cat "$LOG_DIR/alpha2.log" >&2; fail "re-boot did not verify the marker"; }
kill -0 "$A_PID" 2>/dev/null || fail "ds-alpha exited on its own volume"
[ "$(grep created_at= "$MARKER")" = "$STAMP1" ] || fail "re-boot re-stamped the marker"
ok "ds-alpha serves its own volume, stamp stable"
kill "$A_PID" 2>/dev/null; wait "$A_PID" 2>/dev/null

step "ds-beta pointed at ds-alpha's volume must refuse"
mkconfig ds-beta 29492 "$WORK/ds-beta.yaml"
PNFS_MODE=ds "$BIN" --config "$WORK/ds-beta.yaml" > "$LOG_DIR/beta.log" 2>&1 &
B_PID=$!
# The guard fires in DataServer::new — the process must EXIT, nonzero.
for _ in $(seq 1 100); do kill -0 "$B_PID" 2>/dev/null || break; sleep 0.1; done
if kill -0 "$B_PID" 2>/dev/null; then
  cat "$LOG_DIR/beta.log" >&2
  fail "ds-beta is still running on ds-alpha's volume (guard did not fire)"
fi
wait "$B_PID"; B_EXIT=$?
[ "$B_EXIT" -ne 0 ] || fail "ds-beta exited 0 — refusal must be an error"
grep -q "REFUSING to serve" "$LOG_DIR/beta.log" \
  || { cat "$LOG_DIR/beta.log" >&2; fail "refusal message missing from ds-beta log"; }
grep -q "device_id=ds-alpha" "$MARKER" || fail "refusal must not alter the marker"
ok "ds-beta refused (exit $B_EXIT) and the marker is untouched"

printf '\n✅ PASS: DS identity ↔ volume binding guard (stamp, verify, refuse)\n'
