#!/bin/bash
# pNFS read-after-write visibility drill.
#
# Two shipped defects, both found by the DS-lane perf rig's guards on
# 2026-08-31, both with the same blast radius — a client's own data
# vanishing from view:
#
#   1. LAYOUTCOMMIT applied size/mtime to the MDS stub without bumping
#      the F14 change counter, so the attr cache (`8eb18fd4`) kept
#      serving the pre-write size for its full TTL. A client that wrote
#      through its layout and re-opened read its own file as EMPTY
#      (5/5 on lima). GETATTR also served an unmoved CHANGE, so no
#      cache anywhere ever revalidated.
#
#   2. The MDS's startup seed of the device registry dropped the
#      config's controlEndpoint, and the seeded entry satisfies
#      heartbeats — so after an MDS restart the DS never re-registers,
#      truncates of striped files find "no DsControl listener", and
#      LAYOUTGET answers TRYLATER forever: O_TRUNC wedges the client
#      in nfs4_handle_exception indefinitely.
#
# This drill reproduces both shapes against the real MDS+DS pair:
#   phase 1  write→sync→read-back, fresh and reused (O_TRUNC) names
#   phase 2  MDS-only restart, then O_TRUNC + read-back again
#
# Run it against a pre-fix binary to see both failures (phase 1 reads
# come back empty; phase 2 wedges on the timeout).
set -u
VM=${VM:-flint-nfs-client}
MDS_BIN=${MDS_BIN:?set MDS_BIN to an aarch64-musl flint-pnfs-mds path visible in the VM}
DS_BIN=${DS_BIN:?set DS_BIN to an aarch64-musl flint-pnfs-ds path visible in the VM}
MDS_PORT=22590
DS_PORT=22591
DS_CTRL_PORT=23591
GRPC_PORT=52590
vm() { limactl shell "$VM" sudo bash -c "$1"; }

PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); echo "  PASS: $*"; }
bad() { FAIL=$((FAIL+1)); echo "  FAIL: $*"; }

cleanup() {
  vm 'umount -f /mnt/rwv 2>/dev/null; umount -l /mnt/rwv 2>/dev/null
      systemctl stop flint-rwv-mds flint-rwv-mds2 flint-rwv-ds 2>/dev/null
      systemctl reset-failed flint-rwv-mds flint-rwv-mds2 flint-rwv-ds 2>/dev/null' >/dev/null 2>&1
}
trap cleanup EXIT
cleanup

echo "=== SETUP ==="
vm "rm -rf /srv/rwv-mds /srv/rwv-ds1
    mkdir -p /srv/rwv-mds/export /srv/rwv-mds/state /srv/rwv-ds1/data /mnt/rwv
    chmod 0777 /srv/rwv-mds/export
    cat > /srv/rwv-mds/mds.yaml <<CFG
apiVersion: chert.us/v1alpha1
kind: PnfsConfig
mode: mds
mds:
  bind: { address: \"0.0.0.0\", port: $MDS_PORT }
  layout: { type: file, stripeSize: 8388608, policy: stripe }
  dataServers:
    - deviceId: rwv-ds-1
      endpoint: \"127.0.0.1:$DS_PORT\"
      controlEndpoint: \"127.0.0.1:$DS_CTRL_PORT\"
      bdevs: [lvol0]
  state: { backend: sqlite, config: { path: /srv/rwv-mds/state/state.db } }
exports:
  - path: /srv/rwv-mds/export
    fsid: 1
    options: [rw, sync, no_subtree_check]
    access: [{ network: 0.0.0.0/0, permissions: rw }]
logging: { level: warn, format: text }
monitoring:
  prometheus: { enabled: false, port: 0, path: /metrics }
  health: { enabled: false, port: 0, path: /health }
  metrics: []
CFG
    cat > /srv/rwv-ds1/ds.yaml <<CFG
apiVersion: chert.us/v1alpha1
kind: PnfsConfig
mode: ds
ds:
  bind: { address: \"0.0.0.0\", port: $DS_PORT, controlPort: $DS_CTRL_PORT }
  deviceId: rwv-ds-1
  mds:
    endpoint: \"127.0.0.1:$GRPC_PORT\"
    heartbeatInterval: 10
    registrationRetry: 2
    maxRetries: 0
  bdevs:
    - name: lvol0
      mount_point: /srv/rwv-ds1/data
      spdk_volume: lvol0
  resources: { maxConnections: 100, ioQueueDepth: 32, ioBufferSize: 1048576 }
  performance: { useSpdkIo: false, ioThreads: 2, zeroCopy: false }
exports:
  - path: /
    fsid: 1
    options: [rw, sync]
    access: [{ network: 0.0.0.0/0, permissions: rw }]
logging: { level: warn, format: text }
monitoring:
  prometheus: { enabled: false, port: 0, path: /metrics }
  health: { enabled: false, port: 0, path: /health }
  metrics: []
CFG"

vm "systemd-run --unit=flint-rwv-mds --collect --setenv=RUST_LOG=warn \
      --setenv=FLINT_MDS_GRPC_PORT=$GRPC_PORT \
      $MDS_BIN --config /srv/rwv-mds/mds.yaml >/dev/null 2>&1
    sleep 2
    systemd-run --unit=flint-rwv-ds --collect --setenv=RUST_LOG=warn \
      $DS_BIN --config /srv/rwv-ds1/ds.yaml >/dev/null 2>&1"
sleep 4
vm "systemctl is-active --quiet flint-rwv-mds && systemctl is-active --quiet flint-rwv-ds" \
  || { echo "VOID: servers did not start"; exit 1; }
vm "mount -t nfs -o vers=4.1,port=$MDS_PORT,nolock 127.0.0.1:/ /mnt/rwv" \
  || { echo "VOID: mount failed"; exit 1; }

# The reads must actually flow through the DS for this drill to test
# the pNFS lane — a striped write leaves bytes under the DS data dir.
echo "=== ANTI-VACUITY: stripes land on the DS ==="
vm "dd if=/dev/urandom of=/mnt/rwv/seed.bin bs=1M count=8 oflag=direct status=none; sync"
ds_bytes=$(vm "du -sb /srv/rwv-ds1/data | cut -f1" | tr -d '[:space:]')
if [ "${ds_bytes:-0}" -ge $((8 * 1048576)) ]; then
  ok "DS holds $ds_bytes bytes — the lane under test is the pNFS lane"
else
  echo "VOID: stripes did not land on the DS (${ds_bytes:-0} bytes)"; exit 1
fi

echo "=== PHASE 1: write → sync → read-back (attr staleness) ==="
for i in 1 2 3; do
  got=$(vm "echo fresh-$i > /mnt/rwv/fresh-$i.txt && sync && timeout 30 cat /mnt/rwv/fresh-$i.txt" | tr -d '[:space:]')
  [ "$got" = "fresh-$i" ] && ok "fresh file $i reads back its content" \
    || bad "fresh file $i read back [$got] (want fresh-$i)"
done
for i in 1 2 3; do
  got=$(vm "echo reuse-$i > /mnt/rwv/reused.txt && sync && timeout 30 cat /mnt/rwv/reused.txt" | tr -d '[:space:]')
  [ "$got" = "reuse-$i" ] && ok "O_TRUNC rewrite $i reads back its content" \
    || bad "O_TRUNC rewrite $i read back [$got] (want reuse-$i)"
done

echo "=== PHASE 2: MDS-only restart, then O_TRUNC (registry-seed wedge) ==="
vm "systemctl stop flint-rwv-mds; sleep 1
    systemd-run --unit=flint-rwv-mds2 --collect --setenv=RUST_LOG=warn \
      --setenv=FLINT_MDS_GRPC_PORT=$GRPC_PORT \
      $MDS_BIN --config /srv/rwv-mds/mds.yaml >/dev/null 2>&1"
sleep 3
for i in 4 5; do
  if vm "timeout 60 bash -c 'echo reuse-$i > /mnt/rwv/reused.txt' && sync"; then
    got=$(vm "timeout 30 cat /mnt/rwv/reused.txt" | tr -d '[:space:]')
    [ "$got" = "reuse-$i" ] && ok "post-restart O_TRUNC $i reads back its content" \
      || bad "post-restart O_TRUNC $i read back [$got] (want reuse-$i)"
  else
    bad "post-restart O_TRUNC $i WEDGED (the truncate-dirty TRYLATER loop)"
  fi
done

echo
echo "RW-VISIBILITY RESULT: PASS=$PASS FAIL=$FAIL"
[ $FAIL -eq 0 ]
