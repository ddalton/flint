#!/bin/bash
# pNFS Kerberos phase 3 (docs/plans/pnfs-kerberos-plan.md): the data servers
# do Kerberos. Runs INSIDE the krb5 rig VM (setup-kdc.sh + setup-mount.sh
# done), with the binaries in $B. MDS 127.0.0.1:20490 (flintsrv.flint.test),
# DS1 127.0.0.2:20491 (ds1.flint.test), DS2 127.0.0.3:20492 (ds2.flint.test).
# Every arm: MDS floor krb5i, layouts for Kerberos callers granted
# (FLINT_PNFS_LAYOUTS_FOR_GSS=true). Arms:
#   E = krb5i; the DSes have keys (nfs/dsN.flint.test) and the krb5i floor
#   F = krb5p; the same
#   G = krb5i; the DSes keep the krb5i floor, but their principals are NOT in
#       the KDC, so rpc.gssd gets no DS ticket (E1 again, against a DS that
#       refuses the AUTH_SYS the client falls back to)
#   H = krb5i; the DSes have no keytab and no floor (MDS grants layouts
#       anyway: a misconfiguration), so they refuse RPCSEC_GSS
# Per arm: write 64 MiB, read it back, bytes on each DS vs the MDS export,
# the credential flavor of every RPC per port (pcap), what the KDC was asked
# for, and the DSes' refusals.
set -u
B=${B:-/tmp/pnfs-bin}
W=/tmp/pnfs-p3; rm -rf $W; mkdir -p $W
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
# start_servers <arm> <ds-kerberos: keys|nokeys|off>
start_servers() {
  sudo rm -rf $W/mds-export $W/ds1 $W/ds2; sudo mkdir -p $W/mds-export $W/ds1 $W/ds2; sudo chmod 777 $W/mds-export
  sudo env PNFS_MODE=mds KRB5_KTNAME=/tmp/flint-nfs.keytab FLINT_NFS_MIN_SEC=krb5i FLINT_PNFS_LAYOUTS_FOR_GSS=true \
    RUST_LOG=info $B/flint-pnfs-mds --config $W/mds.yaml > $W/mds-$1.log 2>&1 &
  PIDS+=($!); sleep 2
  for n in 1 2; do
    case $2 in
      off) sudo env PNFS_MODE=ds RUST_LOG=info $B/flint-pnfs-ds --config $W/ds$n.yaml > $W/ds$n-$1.log 2>&1 & ;;
      *)   sudo env PNFS_MODE=ds KRB5_KTNAME=/tmp/ds$n.keytab FLINT_NFS_MIN_SEC=krb5i RUST_LOG=info \
             $B/flint-pnfs-ds --config $W/ds$n.yaml > $W/ds$n-$1.log 2>&1 & ;;
    esac
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

arm() {  # arm <name> <sec> <ds: keys|nokeys|off>
  local name=$1 sec=$2 ds=$3
  log ""; log "=== arm $name: sec=$sec, data servers: $ds ==="
  ds_principals $ds
  gssd_on $name
  start_servers $name $ds
  local kts=$(date +%s)
  sudo tcpdump -i lo -s 400 -w $W/cap-$name.pcap 'tcp port 20491 or tcp port 20492 or tcp port 20490' >/dev/null 2>&1 & local tp=$!
  sleep 1
  sudo dmesg -C
  sudo mkdir -p /mnt/pnfs
  local t0=$(date +%s.%N)
  if ! sudo timeout 60 mount -t nfs4 -o vers=4.1,port=20490,sec=$sec flintsrv.flint.test:/ /mnt/pnfs 2>$W/mount-$name.err; then
    log "mount FAILED: $(cat $W/mount-$name.err)"; sudo kill $tp; stop_servers; return
  fi
  log "mounted: $(grep ' /mnt/pnfs ' /proc/mounts | grep -o 'sec=[a-z0-9]*')"
  if [ "$sec" = sys ]; then WR="sudo"; else WR="sudo"; fi
  sudo timeout 180 dd if=$W/src of=/mnt/pnfs/f-$name bs=1M conv=fsync status=none; local wrc=$?
  local t1=$(date +%s.%N)
  sudo sh -c 'echo 3 > /proc/sys/vm/drop_caches'
  local sum=$(sudo timeout 180 md5sum /mnt/pnfs/f-$name | cut -d' ' -f1); local t2=$(date +%s.%N)
  local want=$(md5sum $W/src | cut -d' ' -f1); [ "$sum" = "$want" ] && log "read-back INTACT" || log "read-back CORRUPT: got $sum want $want"
  log "write rc=$wrc  write+fsync $(echo "$t1 $t0" | awk "{printf \"%.1f\", \$1-\$2}") s  read $(echo "$t2 $t1" | awk "{printf \"%.1f\", \$1-\$2}") s  md5 ${sum:-NONE}"
  log "allocated bytes: DS1 $(sudo du -s --block-size=1 $W/ds1 | cut -f1)  DS2 $(sudo du -s --block-size=1 $W/ds2 | cut -f1)  MDS export $(sudo du -s --block-size=1 $W/mds-export | cut -f1)"
  log "layout stats: $(grep -E '^ *(LAYOUTGET|GETDEVICEINFO|LAYOUTRETURN)' /proc/self/mountstats 2>/dev/null | awk '{print $1,$2}' | tr '\n' ' ')"
  sudo umount /mnt/pnfs
  sleep 1; sudo kill $tp; sleep 1
  log "dmesg: $(sudo dmesg | grep -iE 'nfs|rpc|gss|pnfs|layout' | tail -6 | tr '\n' '|')"
  log "KDC asked for: $(sudo journalctl -u krb5-kdc --since @$kts --no-pager -o cat | grep -o 'TGS_REQ[^}]*}, [^ ]* for [^ ,]*' | grep -o 'for [^ ,]*' | sort | uniq -c | tr '\n' ' ')"
  log "KDC unknown: $(sudo journalctl -u krb5-kdc --since @$kts --no-pager -o cat | grep -o 'UNKNOWN_SERVER: [^ ]* for [^ ,]*' | sort | uniq -c | tr '\n' ' ')"
  log "flavors by port: $(sudo python3 /tmp/rpcflav.py $W/cap-$name.pcap)"
  for n in 1 2; do
    log "ds$n: $(grep -m1 -oE 'Security: .*' $W/ds$n-$name.log) | GSS DATA $(grep -c RPCSEC_GSS_DATA $W/ds$n-$name.log) | refused: $(grep -oE 'Refusing sec=[a-z0-9]+|RPCSEC_GSS call refused' $W/ds$n-$name.log | sort | uniq -c | tr '\n' ' ')"
  done
  log "gssd upcalls: $(grep -oE 'service=[a-z]*.*(srchost|target|principal)=?[^ ]*|getting credentials for client with uid [0-9]+ for server [^ ]*' $W/gssd-$name.log 2>/dev/null | sort | uniq -c | head -8 | tr '\n' ' ')"
  stop_servers
}

# The DS principals in the KDC and their keytabs: present for keys/off
# (off: the DS runs without one), absent for nokeys.
ds_principals() {
  for d in 1 2; do sudo kadmin.local -q "delprinc -force nfs/ds$d.flint.test@FLINT.TEST" >/dev/null 2>&1; done
  sudo rm -f /tmp/ds1.keytab /tmp/ds2.keytab
  [ "$1" = nokeys ] && return
  for d in 1 2; do
    sudo kadmin.local -q "addprinc -randkey nfs/ds$d.flint.test@FLINT.TEST" >/dev/null 2>&1
    sudo kadmin.local -q "ktadd -k /tmp/ds$d.keytab nfs/ds$d.flint.test@FLINT.TEST" >/dev/null 2>&1
  done
}

mk_cfgs
dd if=/dev/urandom of=$W/src bs=1M count=64 status=none
for a in $(echo "${ARMS:-EFGH}" | grep -o .); do
  case $a in
    E) arm E krb5i keys ;;
    F) arm F krb5p keys ;;
    G) arm G krb5i nokeys ;;
    H) arm H krb5i off ;;
  esac
done
ds_principals nokeys
gssd_off; sudo systemctl start rpc-gssd 2>/dev/null
log ""; log "=== done ==="
