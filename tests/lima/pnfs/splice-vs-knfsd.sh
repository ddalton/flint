#!/bin/bash
# Three-arm comparison: flint copy path, flint splice path, and knfsd.
#
# WHY A DIFFERENT METRIC THAN S3. S3 read /proc/PID/stat, which works for
# a userspace server and is BLIND to knfsd -- whose work happens in kernel
# threads and softirq context, not in any process this rig owns. Counting
# only flint's process CPU and comparing that to knfsd would flatter flint
# by construction.
#
# So this scores TOTAL SYSTEM busy CPU (/proc/stat, all fields minus idle
# and iowait) across an identical workload. That counts everything: the
# server, the NFS client, TCP, softirq. It is the only number that means
# the same thing for an in-kernel server and a userspace one.
#
# The cost of that choice, stated plainly: the client's share is included
# in every arm, so differences are DILUTED relative to a server-only
# measurement. This metric understates the gap; it does not invent one.
#
# Cross-session comparison is what this exists to avoid. Recorded knfsd
# figures come from other sessions and build profiles, and cpu-ms/GiB
# already misled once this month when knfsd's own number moved 560 -> 400
# between runs. All three arms here run in ONE session on ONE kernel.
set -u
VM=${VM:-flint-nfs-client}
BIN=${BIN:-/tmp/flint-splice-mds}
REPS=${REPS:-5}
READERS=${READERS:-4}
SIZE_MB=${SIZE_MB:-64}
PASSES=${PASSES:-8}
PORT_OFF=${PORT_OFF:-20881}
PORT_ON=${PORT_ON:-20882}
vm() { limactl shell "$VM" sudo bash -c "$1"; }

cleanup() {
  limactl shell "${VM:-flint-nfs-client}" sudo bash -c \
    'umount -l /mnt/koff 2>/dev/null||true; umount -l /mnt/kon 2>/dev/null||true; umount -l /mnt/knfsd 2>/dev/null||true' >/dev/null 2>&1
}
trap cleanup EXIT

mkfiles() { vm "for i in \$(seq 1 $READERS); do dd if=/dev/urandom of=$1/f\$i.bin bs=1M count=$SIZE_MB status=none; done; echo '$2' > $1/WHICH_ARM"; }

setup_flint() { # name port splice_env
  local n=$1 p=$2 sp=$3
  vm "umount -l /mnt/$n 2>/dev/null||true
      systemctl stop flint-$n 2>/dev/null||true
      rm -rf /srv/$n /mnt/$n; mkdir -p /srv/$n/export /srv/$n/state /mnt/$n
      chmod 0777 /srv/$n/export"
  mkfiles "/srv/$n/export" "$n"
  vm "cat > /srv/$n/mds.yaml <<CFG
apiVersion: flint.io/v1alpha1
kind: PnfsConfig
mode: standalone
mds:
  bind: { address: \"0.0.0.0\", port: $p }
  layout: { type: file, stripeSize: 8388608, policy: stripe }
  dataServers: []
  state: { backend: sqlite, config: { path: /srv/$n/state/state.db } }
exports:
  - path: /srv/$n/export
    fsid: 1
    options: [rw, sync, no_subtree_check]
    access: [{ network: 0.0.0.0/0, permissions: rw }]
logging: { level: warn, format: text, components: { mds: warn } }
monitoring:
  prometheus: { enabled: false, port: 0, path: /metrics }
  health: { enabled: false, port: 0, path: /health }
  metrics: []
CFG
      systemd-run --unit=flint-$n --collect --setenv=RUST_LOG=warn $sp \
        $BIN --config /srv/$n/mds.yaml >/dev/null 2>&1"
  sleep 4
  vm "systemctl is-active --quiet flint-$n" || { echo "VOID: flint-$n did not start"; exit 1; }
  vm "mount -t nfs -o vers=4.1,port=$p,nolock 127.0.0.1:/ /mnt/$n" >/dev/null 2>&1
}

setup_knfsd() {
  vm "umount -l /mnt/knfsd 2>/dev/null||true
      rm -rf /srv/knfsd-splice; mkdir -p /srv/knfsd-splice /mnt/knfsd
      chmod 0777 /srv/knfsd-splice"
  mkfiles "/srv/knfsd-splice" "knfsd"
  vm "grep -q /srv/knfsd-splice /etc/exports 2>/dev/null || \
        echo '/srv/knfsd-splice 127.0.0.1/32(rw,sync,no_subtree_check,no_root_squash,fsid=9)' >> /etc/exports
      exportfs -ra; systemctl restart nfs-kernel-server; sleep 3
      mount -t nfs -o vers=4.1,nolock 127.0.0.1:/srv/knfsd-splice /mnt/knfsd" >/dev/null 2>&1
}

# Total system busy ticks: everything except idle and iowait.
sys_busy() { vm "awk '/^cpu /{idle=\$5+\$6; t=0; for(i=2;i<=NF;i++) t+=\$i; print t-idle}' /proc/stat" | tr -d '[:space:]'; }

run_arm() { # mountpoint -> "busy_ms wall_ms bytes"
  local m=$1 b0 b1 w0 w1
  b0=$(sys_busy); w0=$(date +%s%N)
  vm "for p in \$(seq 1 $PASSES); do
        for i in \$(seq 1 $READERS); do
          dd if=$m/f\$i.bin of=/dev/null bs=1M count=$SIZE_MB iflag=direct status=none &
        done; wait
      done" >/dev/null 2>&1
  w1=$(date +%s%N); b1=$(sys_busy)
  local tck; tck=$(vm "getconf CLK_TCK" | tr -d '[:space:]')
  echo "$(( (b1 - b0) * 1000 / tck )) $(( (w1 - w0) / 1000000 )) $(( READERS * SIZE_MB * PASSES * 1048576 ))"
}

echo "=== VM HEALTH BEFORE ==="; vm "uptime; vmstat 1 2 | tail -1"
echo "=== SETUP ==="
setup_flint koff "$PORT_OFF" ""
setup_flint kon  "$PORT_ON"  "--setenv=FLINT_NFS_SPLICE=1"
setup_knfsd
for a in koff kon knfsd; do
  mp=/mnt/$a
  who=$(vm "cat $mp/WHICH_ARM 2>/dev/null" | tr -d '[:space:]')
  exp=$a; [ "$a" = "knfsd" ] && exp=knfsd
  [ "$who" = "$exp" ] || { echo "VOID: $mp serving '$who', expected '$exp'"; exit 1; }
  src=$(vm "md5sum /srv/$( [ "$a" = knfsd ] && echo knfsd-splice || echo "$a/export" )/f1.bin | cut -d' ' -f1" | tr -d '[:space:]')
  via=$(vm "dd if=$mp/f1.bin bs=1M iflag=direct status=none | md5sum | cut -d' ' -f1" | tr -d '[:space:]')
  [ "$src" = "$via" ] || { echo "VOID: $a served bytes that differ from disk"; exit 1; }
  echo "  $a: identity + content verified"
done

echo "=== EXECUTION GUARD (splice must fire on kon, and ONLY there) ==="
for a in koff kon; do
  n=$(vm "ls -l /proc/\$(systemctl show -p MainPID --value flint-$a)/fd 2>/dev/null | grep -c pipe || true" | tr -d '[:space:]')
  echo "  flint-$a pipe fds: $n"
done

echo "=== WARM ==="; for a in koff kon knfsd; do run_arm /mnt/$a >/dev/null; done

echo "=== REPS (interleaved) ==="
echo "rep,arm,busy_ms,wall_ms,bytes" > /tmp/s4.csv
for r in $(seq 1 "$REPS"); do
  for a in koff kon knfsd; do
    read -r ms wms by <<<"$(run_arm /mnt/$a)"
    echo "$r,$a,$ms,$wms,$by" >> /tmp/s4.csv
    echo "  rep $r $a: ${ms}ms system-cpu, ${wms}ms wall, $((by/1048576))MiB"
  done
done
echo "=== VM HEALTH AFTER ==="; vm "vmstat 1 2 | tail -1"
echo "=== CSV ==="; cat /tmp/s4.csv
