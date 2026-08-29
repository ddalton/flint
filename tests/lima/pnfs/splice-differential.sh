#!/bin/bash
# S3: does splice staging actually reduce SERVER CPU per byte served?
#
# A/B of ONE binary with FLINT_NFS_SPLICE toggled. Not flint-vs-knfsd:
# the question here is whether the new path is cheaper than the old one.
#
# SCORED AS cpu-ms/GiB, NOT MiB/s. S0 measured a 72% CPU reduction that
# produced only ~21% more throughput, and in 2 of 5 reps splice's wall
# time was WORSE. A throughput gate would read ~1.0 here and be called
# "no effect" -- which is exactly the shape of the 0.989x null that once
# cost a day. The win is CPU, which cashes out as concurrency headroom.
#
# GUARDS, because a favourable number is not evidence on its own:
#   1. IDENTITY  -- each mount must be attached to the server it names.
#                   Two units once bound the same port and a mount
#                   silently attached to the OTHER server.
#   2. EXECUTION -- the ON arm must actually splice. A pipe is created
#                   only by the splice pool, so pipe fds in /proc/PID/fd
#                   are a DIRECT observation of the mechanism. Flag-on
#                   and flag-off measuring the same is indistinguishable
#                   from the path never firing.
#   3. RIG HEALTH-- VM idle before and after. A runaway process once made
#                   a gate PASS with a flattering ratio.
set -u
VM=${VM:-flint-nfs-client}
# Leave no stale mount behind. Without this, the NEXT run inherits a mount
# whose server it is about to kill, which is how the wedge above happened.
cleanup() {
  limactl shell "${VM:-flint-nfs-client}" sudo bash -c \
    'umount -l /mnt/off 2>/dev/null||true; umount -l /mnt/on 2>/dev/null||true' >/dev/null 2>&1
}
trap cleanup EXIT
BIN=${BIN:-/tmp/flint-splice-mds}
REPS=${REPS:-5}
READERS=${READERS:-4}
SIZE_MB=${SIZE_MB:-64}
# Passes over the SAME files per measurement. CPU is sampled in CLK_TCK
# ticks (10ms), so a short run is quantisation-limited: 3 ticks vs 7 gives
# a ratio with +/-33% of slop. Growing the working set instead would
# exceed the VM's 2 GiB RAM, push reads to disk, and dilute the CPU signal
# into I/O wait -- so accumulate ticks by re-reading a cache-warm set.
PASSES=${PASSES:-8}
PORT_OFF=${PORT_OFF:-20871}
PORT_ON=${PORT_ON:-20872}
vm() { limactl shell "$VM" sudo bash -c "$1"; }

setup_arm() { # name port splice_env
  local n=$1 p=$2 sp=$3
  # ORDER MATTERS, and getting it wrong wedges the VM. Unmount BEFORE
  # stopping the server: an NFS mount whose server is already gone cannot
  # be unmounted with -f, which blocks in D-state forever (seen: 286s and
  # climbing, load 1.00 at 0% CPU). `umount -l` detaches immediately
  # regardless of server state, so it is the only safe form here.
  vm "umount -l /mnt/$n 2>/dev/null||true
      systemctl stop flint-$n 2>/dev/null||true
      rm -rf /srv/$n /mnt/$n; mkdir -p /srv/$n/export /srv/$n/state /mnt/$n
      chmod 0777 /srv/$n/export
      # IDENTITY marker: names the arm, so a mount pointed at the wrong
      # server is caught rather than silently measured.
      echo '$n' > /srv/$n/export/WHICH_ARM
      for i in \$(seq 1 $READERS); do
        dd if=/dev/urandom of=/srv/$n/export/f\$i.bin bs=1M count=$SIZE_MB status=none
      done
      cat > /srv/$n/mds.yaml <<CFG
apiVersion: flint.io/v1alpha1
kind: PnfsConfig
mode: standalone
mds:
  bind:
    address: \"0.0.0.0\"
    port: $p
  layout:
    type: file
    stripeSize: 8388608
    policy: stripe
  dataServers: []
  state:
    backend: sqlite
    config:
      path: /srv/$n/state/state.db
exports:
  - path: /srv/$n/export
    fsid: 1
    options: [rw, sync, no_subtree_check]
    access:
      - network: 0.0.0.0/0
        permissions: rw
logging:
  level: warn
  format: text
  components:
    mds: warn
monitoring:
  prometheus: { enabled: false, port: 0, path: /metrics }
  health: { enabled: false, port: 0, path: /health }
  metrics: []
CFG
      systemd-run --unit=flint-$n --collect --setenv=RUST_LOG=warn $sp \
        $BIN --config /srv/$n/mds.yaml >/dev/null 2>&1"
  sleep 4
  if ! vm "systemctl is-active --quiet flint-$n"; then
    echo "VOID: unit flint-$n did not start"; vm "journalctl -u flint-$n -n 15 --no-pager"; exit 1
  fi
  vm "mount -t nfs -o vers=4.1,port=$p,nolock 127.0.0.1:/ /mnt/$n" >/dev/null 2>&1
  # GUARD 1
  local who; who=$(vm "cat /mnt/$n/WHICH_ARM 2>/dev/null" | tr -d '[:space:]')
  if [ "$who" != "$n" ]; then
    echo "VOID: /mnt/$n is serving '$who', not '$n' -- wrong server"; exit 1
  fi
  echo "  arm $n: up on :$p, identity verified"
}

cpu_ticks() { vm "awk '{print \$14+\$15}' /proc/\$(systemctl show -p MainPID --value flint-$1)/stat"; }
pipe_fds()  { vm "ls -l /proc/\$(systemctl show -p MainPID --value flint-$1)/fd 2>/dev/null | grep -c pipe || true"; }

run_arm() { # name -> "cpu_ms bytes"
  local n=$1
  local t0 t1 w0 w1; t0=$(cpu_ticks "$n")
  w0=$(date +%s%N)
  vm "for p in \$(seq 1 $PASSES); do
        for i in \$(seq 1 $READERS); do
          dd if=/mnt/$n/f\$i.bin of=/dev/null bs=1M count=$SIZE_MB iflag=direct status=none &
        done; wait
      done" >/dev/null 2>&1
  w1=$(date +%s%N)
  t1=$(cpu_ticks "$n")
  local tck; tck=$(vm "getconf CLK_TCK" | tr -d '[:space:]')
  # cpu_ms, wall_ms, bytes -- wall is measured from the host, so it
  # includes limactl round-trip overhead and is a FLOOR on throughput,
  # not a precise figure. Same overhead on both arms, so the RATIO is
  # the meaningful part.
  echo "$(( (t1 - t0) * 1000 / tck )) $(( (w1 - w0) / 1000000 )) $(( READERS * SIZE_MB * PASSES * 1048576 ))"
}

echo "=== VM HEALTH BEFORE ==="; vm "uptime; vmstat 1 2 | tail -1"
echo "=== SETUP ==="
setup_arm off "$PORT_OFF" ""
setup_arm on  "$PORT_ON"  "--setenv=FLINT_NFS_SPLICE=1"

echo "=== WARM (fills the server page cache; the measured reps are warm) ==="
run_arm off >/dev/null; run_arm on >/dev/null

# GUARD 2a: CORRECTNESS. `dd of=/dev/null` never looks at the bytes, so a
# path serving short or garbled reads would score as FAST rather than
# broken. This is the first time the splice path has served a real NFS
# client, so verify content end to end before believing any ratio.
echo "=== GUARD 2a: CORRECTNESS ==="
for a in off on; do
  src=$(vm "md5sum /srv/$a/export/f1.bin | cut -d' ' -f1" | tr -d '[:space:]')
  via=$(vm "dd if=/mnt/$a/f1.bin bs=1M iflag=direct status=none | md5sum | cut -d' ' -f1" | tr -d '[:space:]')
  sz_src=$(vm "stat -c %s /srv/$a/export/f1.bin" | tr -d '[:space:]')
  sz_via=$(vm "dd if=/mnt/$a/f1.bin bs=1M iflag=direct status=none | wc -c" | tr -d '[:space:]')
  echo "  arm $a: disk=$src  served=$via  bytes=$sz_via/$sz_src"
  if [ "$src" != "$via" ] || [ "$sz_src" != "$sz_via" ]; then
    echo "  *** VOID: arm $a served bytes that do not match the file on disk."
    echo "      A fast path that serves wrong data is a defect, not a result."
    exit 3
  fi
done
echo "  both arms serve byte-identical content"

echo "=== GUARD 2: EXECUTION ==="
P_OFF=$(pipe_fds off); P_ON=$(pipe_fds on)
echo "  pipe fds -- off:$P_OFF  on:$P_ON"
if [ "${P_ON:-0}" -le "${P_OFF:-0}" ]; then
  echo "  *** VOID: the ON arm holds no more pipes than OFF -- splice did NOT fire."
  echo "      Any ratio below would be measuring the same code path twice."
  exit 2
fi
echo "  splice confirmed active"

echo "=== REPS (interleaved) ==="
echo "rep,arm,cpu_ms,wall_ms,bytes" > /tmp/s3.csv
for r in $(seq 1 "$REPS"); do
  for a in off on; do
    read -r ms wms by <<<"$(run_arm $a)"
    echo "$r,$a,$ms,$wms,$by" >> /tmp/s3.csv
    echo "  rep $r $a: ${ms}ms cpu, ${wms}ms wall for $((by/1048576))MiB  ($(( by/1048576 * 1000 / (wms>0?wms:1) )) MiB/s)"
  done
done
echo "=== VM HEALTH AFTER ==="; vm "vmstat 1 2 | tail -1"
echo "=== CSV ==="; cat /tmp/s3.csv
