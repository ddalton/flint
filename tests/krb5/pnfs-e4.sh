#!/bin/bash
# pNFS Kerberos plan, experiment E4 (SCRATCH, branch e4-scratch): which
# data server error for a refused filehandle does the Linux client recover
# from cleanly -- a new LAYOUTGET, or I/O through the MDS -- and which
# livelocks. The DS refuses EVERY PUTFH with FLINT_E4_REFUSE's status.
# sec=sys (no Kerberos needed). Per arm: 16 MiB written and read back with
# timeouts, wall time, the DS refusal count (a livelock is a large count),
# bytes on the DSes vs the MDS, dmesg.
set -u
B=${B:-/tmp/pnfs-bin}
W=/tmp/pnfs-e4; rm -rf $W; mkdir -p $W
OUT=$W/results.txt
log() { echo "$*" | tee -a $OUT; }

grep -q ds1.flint.test /etc/hosts || printf '127.0.0.2 ds1.flint.test\n127.0.0.3 ds2.flint.test\n' | sudo tee -a /etc/hosts >/dev/null

mk_cfgs() {
  cat > $W/mds.yaml <<EOF
apiVersion: chert.us/v1alpha1
kind: PnfsConfig
mode: mds
mds:
  bind: {address: "0.0.0.0", port: 20490}
  layout: {type: file, stripeSize: 1048576, policy: stripe}
  dataServers:
    - {deviceId: ds-1, endpoint: "127.0.0.2:20491", controlEndpoint: "127.0.0.1:21491", bdevs: [lvol0]}
    - {deviceId: ds-2, endpoint: "127.0.0.3:20492", controlEndpoint: "127.0.0.1:21492", bdevs: [lvol0]}
  state: {backend: memory, config: {}}
  ha: {enabled: false, replicas: 1, leaderElection: false, leaseDuration: 15, renewDeadline: 10, retryPeriod: 2}
  failover: {heartbeatTimeout: 30, policy: recall_affected, gracePeriod: 60}
exports:
  - {path: $W/mds-export, fsid: 1, options: [rw, sync, no_subtree_check], access: [{network: 0.0.0.0/0, permissions: rw}]}
logging: {level: info, format: text, components: {mds: info, layout: debug}}
monitoring: {prometheus: {enabled: false, port: 0, path: /metrics}, health: {enabled: false, port: 0, path: /health}, metrics: []}
development: {debug: false, simulateFailures: {enabled: false, interval: 0, duration: 0}, traceRpc: false, dumpLayouts: false}
EOF
  for n in 1 2; do
    cat > $W/ds$n.yaml <<EOF
apiVersion: chert.us/v1alpha1
kind: PnfsConfig
mode: ds
ds:
  bind: {address: "0.0.0.0", port: 2049$n, controlPort: 2149$n}
  deviceId: ds-$n
  mds: {endpoint: "127.0.0.1:50051", heartbeatInterval: 10, registrationRetry: 5, maxRetries: 0}
  bdevs: [{name: lvol0, mount_point: $W/ds$n, spdk_volume: lvol0}]
  resources: {maxConnections: 100, ioQueueDepth: 32, ioBufferSize: 1048576}
  performance: {useSpdkIo: false, ioThreads: 2, zeroCopy: false}
exports: [{path: /, fsid: 1, options: [rw, sync], access: [{network: 0.0.0.0/0, permissions: rw}]}]
logging: {level: info, format: text, components: {ds: info, io: info}}
monitoring: {prometheus: {enabled: false, port: 0, path: /metrics}, health: {enabled: false, port: 0, path: /health}, metrics: []}
development: {debug: false, simulateFailures: {enabled: false, interval: 0, duration: 0}, traceRpc: false, dumpLayouts: false}
EOF
  done
}

PIDS=()
# start_servers <arm> <refuse-status or none>
start_servers() {
  sudo rm -rf $W/mds-export $W/ds1 $W/ds2; sudo mkdir -p $W/mds-export $W/ds1 $W/ds2; sudo chmod 777 $W/mds-export
  sudo env PNFS_MODE=mds RUST_LOG=info $B/flint-pnfs-mds --config $W/mds.yaml > $W/mds-$1.log 2>&1 &
  PIDS+=($!); sleep 2
  for n in 1 2; do
    sudo env PNFS_MODE=ds FLINT_E4_REFUSE=$2 RUST_LOG=info $B/flint-pnfs-ds --config $W/ds$n.yaml > $W/ds$n-$1.log 2>&1 &
    PIDS+=($!)
  done
  sleep 6
}
stop_servers() {
  sudo umount -lf /mnt/pnfs 2>/dev/null
  for p in "${PIDS[@]}"; do sudo kill "$p" 2>/dev/null; done
  # sudo's children: kill by our exact config paths, never by binary name
  for f in $W/mds.yaml $W/ds1.yaml $W/ds2.yaml; do sudo pkill -f -- "--config $f" 2>/dev/null; done
  PIDS=(); sleep 2
}

gssd_on()  { sudo systemctl stop rpc-gssd 2>/dev/null; sudo pkill -x rpc.gssd; sleep 1; sudo /usr/sbin/rpc.gssd -f -vvv > $W/gssd-$1.log 2>&1 & sleep 1; }
gssd_off() { sudo systemctl stop rpc-gssd 2>/dev/null; sudo pkill -x rpc.gssd; sleep 1; }

arm() {  # arm <name> <refuse-status>
  local name=$1 st=$2
  log ""; log "=== arm $name: DS PUTFH refused with $st ==="
  gssd_off
  start_servers $name $st
  sudo dmesg -C
  sudo mkdir -p /mnt/pnfs
  if ! sudo timeout 60 mount -t nfs4 -o vers=4.1,port=20490,sec=sys flintsrv.flint.test:/ /mnt/pnfs 2>$W/mount-$name.err; then
    log "mount FAILED: $(cat $W/mount-$name.err)"; stop_servers; return
  fi
  local t0=$(date +%s.%N)
  sudo timeout 120 dd if=$W/src of=/mnt/pnfs/f-$name bs=1M conv=fsync status=none; local wrc=$?
  local t1=$(date +%s.%N)
  sudo sh -c 'echo 3 > /proc/sys/vm/drop_caches'
  local sum=$(sudo timeout 120 md5sum /mnt/pnfs/f-$name | cut -d' ' -f1); local rrc=$?; local t2=$(date +%s.%N)
  local want=$(md5sum $W/src | cut -d' ' -f1); [ "$sum" = "$want" ] && log "read-back INTACT" || log "read-back BAD (rc $rrc): got ${sum:-NONE}"
  log "write rc=$wrc (124 = timed out)  write+fsync $(echo "$t1 $t0" | awk "{printf \"%.1f\", \$1-\$2}") s  read $(echo "$t2 $t1" | awk "{printf \"%.1f\", \$1-\$2}") s"
  log "allocated bytes: DS1 $(sudo du -s --block-size=1 $W/ds1 | cut -f1)  DS2 $(sudo du -s --block-size=1 $W/ds2 | cut -f1)  MDS export $(sudo du -s --block-size=1 $W/mds-export | cut -f1)"
  sleep 12
  for n in 1 2; do log "ds$n refusals: $(grep -o 'E4 refused so far: [0-9]*' $W/ds$n-$name.log | tail -1)"; done
  log "MDS LAYOUTGETs: $(grep -c 'LAYOUTGET' $W/mds-$name.log)  layoutreturns: $(grep -ci 'LAYOUTRETURN' $W/mds-$name.log)"
  log "dmesg: $(sudo dmesg | grep -iE 'nfs|rpc|pnfs|layout' | tail -6 | tr '\n' '|')"
  sudo timeout 30 umount /mnt/pnfs || sudo umount -lf /mnt/pnfs
  stop_servers
}

mk_cfgs
dd if=/dev/urandom of=$W/src bs=1M count=16 status=none
for st in ${E4_ARMS:-none badhandle stale access perm io fhexpired}; do arm $st $st; done
sudo systemctl start rpc-gssd 2>/dev/null
log ""; log "=== done ==="
