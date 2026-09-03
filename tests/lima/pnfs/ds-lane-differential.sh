#!/bin/bash
# DS-lane differential: the pNFS data-server READ path vs knfsd, with the
# standalone MDS as a same-binary calibration arm.
#
# WHY THIS EXISTS. The repo carried a "2.13x DS deficit" (396 MiB/s over
# pNFS vs 845 local block, runaw 2026-08-01) whose provenance is a code
# comment, and every read-path fix since (reply segmenting, the buffer
# pool, fore-channel headroom, splice) landed on — and was measured on —
# the STANDALONE lane. The DS lane got exactly one of them (the pool,
# `5863468e`), applied blind. This rig is the first instrument that can
# see the DS lane at all.
#
# Three arms, one VM session, interleaved per rep (drift is common-mode):
#
#   pnfs   flint-pnfs-mds (mode: mds) + one flint-pnfs-ds; the client
#          mounts the MDS with vers=4.1 and file-layout READs go to the DS.
#   solo   flint-pnfs-mds (mode: standalone) — every campaign fix, same
#          session. pnfs/solo isolates the DS fork; solo/knfsd calibrates
#          this run against the recorded campaign numbers.
#   knfsd  the kernel server, the control.
#
# Scored like splice-vs-knfsd.sh: TOTAL system busy CPU (/proc/stat minus
# idle+iowait) and wall time over an identical workload — the only metric
# that means the same thing for an in-kernel server and a userspace one.
# Per-process CPU for the MDS and DS is recorded per rep as a diagnostic
# AND as the execution guard: if READs were served by the MDS fallback
# instead of the DS, the DS would sit idle and the rep is VOID.
#
# After scoring, a diagnostic pass counts RPCs on a fresh DS connection
# for one 64 MiB file: ~64+setup RPCs means clean 1 MiB READs; ~130 means
# the fore-channel runt split (the exact-1MiB advertisement bug the
# standalone lane fixed in `3104fc51`).
set -u
VM=${VM:-flint-nfs-client}
MDS_BIN=${MDS_BIN:?set MDS_BIN to an aarch64-musl flint-pnfs-mds path visible in the VM}
DS_BIN=${DS_BIN:?set DS_BIN to an aarch64-musl flint-pnfs-ds path visible in the VM}
REPS=${REPS:-5}
READERS=${READERS:-4}
SIZE_MB=${SIZE_MB:-64}
PASSES=${PASSES:-8}
MDS_PORT=22490
DS_PORT=22491
DS_CTRL_PORT=23491
GRPC_PORT=52490
SOLO_PORT=22492
vm() { limactl shell "$VM" sudo bash -c "$1"; }

cleanup() {
  limactl shell "$VM" sudo bash -c '
    umount -l /mnt/dsl-pnfs /mnt/dsl-solo /mnt/dsl-knfsd 2>/dev/null || true
    systemctl stop flint-dsl-mds flint-dsl-ds flint-dsl-solo 2>/dev/null || true
  ' >/dev/null 2>&1
}
trap cleanup EXIT

echo "=== VM HEALTH BEFORE ==="
vm "uptime; ps -eo pcpu,comm --sort=-pcpu | head -4; df -h / | tail -1"

echo "=== SETUP ==="
cleanup
vm "systemctl reset-failed flint-dsl-mds flint-dsl-ds flint-dsl-solo 2>/dev/null || true
    rm -rf /srv/dsl-mds /srv/dsl-ds1 /srv/dsl-solo
    # The knfsd export root must SURVIVE between runs: deleting and
    # recreating the exported directory changes its inode, and a
    # lingering client superblock from the previous run then resolves
    # the root to the OLD inode — every new file is invisible (ENOENT)
    # through the fresh mount. Wipe contents, keep the directory.
    mkdir -p /srv/dsl-knfsd
    rm -rf /srv/dsl-knfsd/* 2>/dev/null || true
    mkdir -p /srv/dsl-mds/export /srv/dsl-mds/state /srv/dsl-ds1 \
             /srv/dsl-solo/export /srv/dsl-solo/state \
             /mnt/dsl-pnfs /mnt/dsl-solo /mnt/dsl-knfsd
    chmod 0777 /srv/dsl-mds/export /srv/dsl-solo/export /srv/dsl-knfsd"

# ── configs ───────────────────────────────────────────────────────────
vm "cat > /srv/dsl-mds/mds.yaml <<CFG
apiVersion: chert.us/v1alpha1
kind: PnfsConfig
mode: mds
mds:
  bind: { address: \"0.0.0.0\", port: $MDS_PORT }
  layout: { type: file, stripeSize: 8388608, policy: stripe }
  dataServers:
    - deviceId: ds-lane-1
      endpoint: \"127.0.0.1:$DS_PORT\"
      controlEndpoint: \"127.0.0.1:$DS_CTRL_PORT\"
      bdevs: [lvol0]
  state: { backend: sqlite, config: { path: /srv/dsl-mds/state/state.db } }
exports:
  - path: /srv/dsl-mds/export
    fsid: 1
    options: [rw, sync, no_subtree_check]
    access: [{ network: 0.0.0.0/0, permissions: rw }]
logging: { level: warn, format: text }
monitoring:
  prometheus: { enabled: false, port: 0, path: /metrics }
  health: { enabled: false, port: 0, path: /health }
  metrics: []
CFG
cat > /srv/dsl-ds1/ds.yaml <<CFG
apiVersion: chert.us/v1alpha1
kind: PnfsConfig
mode: ds
ds:
  bind: { address: \"0.0.0.0\", port: $DS_PORT, controlPort: $DS_CTRL_PORT }
  deviceId: ds-lane-1
  mds:
    endpoint: \"127.0.0.1:$GRPC_PORT\"
    heartbeatInterval: 10
    registrationRetry: 2
    maxRetries: 0
  bdevs:
    - name: lvol0
      mount_point: /srv/dsl-ds1/data
      spdk_volume: lvol0
  resources: { maxConnections: 100, ioQueueDepth: 32, ioBufferSize: 1048576 }
  performance: { useSpdkIo: false, ioThreads: 2, zeroCopy: false }
exports:
  - path: /
    fsid: 1
    options: [rw, sync]
    access: [{ network: 0.0.0.0/0, permissions: rw }]
logging: { level: info, format: text }
monitoring:
  prometheus: { enabled: false, port: 0, path: /metrics }
  health: { enabled: false, port: 0, path: /health }
  metrics: []
CFG
cat > /srv/dsl-solo/mds.yaml <<CFG
apiVersion: chert.us/v1alpha1
kind: PnfsConfig
mode: standalone
mds:
  bind: { address: \"0.0.0.0\", port: $SOLO_PORT }
  layout: { type: file, stripeSize: 8388608, policy: stripe }
  dataServers: []
  state: { backend: sqlite, config: { path: /srv/dsl-solo/state/state.db } }
exports:
  - path: /srv/dsl-solo/export
    fsid: 1
    options: [rw, sync, no_subtree_check]
    access: [{ network: 0.0.0.0/0, permissions: rw }]
logging: { level: warn, format: text }
monitoring:
  prometheus: { enabled: false, port: 0, path: /metrics }
  health: { enabled: false, port: 0, path: /health }
  metrics: []
CFG
mkdir -p /srv/dsl-ds1/data"

# ── servers up ────────────────────────────────────────────────────────
vm "systemd-run --unit=flint-dsl-mds --collect \
      --setenv=RUST_LOG=warn --setenv=FLINT_MDS_GRPC_PORT=$GRPC_PORT \
      $MDS_BIN --config /srv/dsl-mds/mds.yaml >/dev/null 2>&1
    systemd-run --unit=flint-dsl-solo --collect --setenv=RUST_LOG=warn \
      $MDS_BIN --config /srv/dsl-solo/mds.yaml >/dev/null 2>&1
    sleep 2
    systemd-run --unit=flint-dsl-ds --collect --setenv=RUST_LOG=info \
      $DS_BIN --config /srv/dsl-ds1/ds.yaml >/dev/null 2>&1"
sleep 3
for u in flint-dsl-mds flint-dsl-ds flint-dsl-solo; do
  vm "systemctl is-active --quiet $u" || {
    echo "VOID: $u did not start"; vm "journalctl -u $u --no-pager | tail -15"; exit 1
  }
done

# DS must be REGISTERED before a layout can point at it.
reg=""
for _ in $(seq 1 30); do
  reg=$(vm "journalctl -u flint-dsl-ds --no-pager 2>/dev/null | grep -ci 'regist' || true" | tr -d '[:space:]')
  [ "${reg:-0}" -ge 1 ] && break
  sleep 1
done
echo "  DS registration log lines: ${reg:-0}"

# knfsd control. The exports line is REPLACED, not appended-if-absent:
# knfsd resolves filehandles BY FSID, and this VM's exports file
# accumulates rig lines — a duplicate fsid means a mount of THIS path
# silently serves ANOTHER rig's directory (measured: fsid=11 was
# claimed three times and this mount served the write-rig's files;
# the identity guard is what caught it). fsid=77 is this drill's own.
vm "sed -i '\\#^/srv/dsl-knfsd #d' /etc/exports
    echo '/srv/dsl-knfsd 127.0.0.1/32(rw,sync,no_subtree_check,no_root_squash,fsid=77)' >> /etc/exports
    exportfs -ra; systemctl restart nfs-kernel-server; sleep 2"

# ── mounts ────────────────────────────────────────────────────────────
vm "mount -t nfs -o vers=4.1,port=$MDS_PORT,nolock 127.0.0.1:/ /mnt/dsl-pnfs" \
  || { echo "VOID: pnfs mount failed"; vm "journalctl -u flint-dsl-mds --no-pager | tail -10"; exit 1; }
vm "mount -t nfs -o vers=4.1,port=$SOLO_PORT,nolock 127.0.0.1:/ /mnt/dsl-solo" \
  || { echo "VOID: solo mount failed"; exit 1; }
vm "mount -t nfs -o vers=4.1,nolock 127.0.0.1:/srv/dsl-knfsd /mnt/dsl-knfsd" \
  || { echo "VOID: knfsd mount failed"; exit 1; }
for m in dsl-pnfs dsl-solo dsl-knfsd; do
  vm "findmnt -n -t nfs4 /mnt/$m >/dev/null" || { echo "VOID: /mnt/$m is not nfs4"; exit 1; }
done

# ── test files ────────────────────────────────────────────────────────
# Each arm gets its OWN random source, so the md5-through-the-mount
# check below is the arm-identity guard: a mount aliased to the wrong
# server serves recognizably wrong bytes. (An earlier WHICH_ARM text
# file was read through the pNFS mount and hit the LAYOUTCOMMIT
# attr-staleness bug this campaign fixed — big O_DIRECT reads are the
# workload, so the guard reads the same way the workload does.)
# The pnfs arm's copies go THROUGH the mount so the stripes actually
# land on the DS (server-side creation would leave the DS empty and
# the MDS serving everything — the exact failure this rig exists to
# detect). solo/knfsd get server-side copies like the splice rig;
# their read path does not depend on how the file arrived.
# Files live under a RUN-UNIQUE directory: back-to-back runs of this
# drill remount the same exports, and the kernel client can hand the
# new mount the OLD superblock — whose cached dentries point at the
# previous run's (deleted) inodes and read back ESTALE/empty. A fresh
# directory name makes every lookup fresh no matter what a lingering
# superblock remembers.
RUNTAG="run-$(date +%s)-$$"
vm "mkdir -p /mnt/dsl-pnfs/$RUNTAG /srv/dsl-solo/export/$RUNTAG /srv/dsl-knfsd/$RUNTAG" \
  || { echo "VOID: run-dir creation failed"; exit 1; }
vm "for a in pnfs solo knfsd; do dd if=/dev/urandom of=/var/tmp/dsl-src-\$a.bin bs=1M count=$SIZE_MB status=none; done"
vm "for i in \$(seq 1 $READERS); do
      dd if=/var/tmp/dsl-src-pnfs.bin of=/mnt/dsl-pnfs/$RUNTAG/f\$i.bin bs=1M oflag=direct status=none || exit 1
    done; sync" \
  || { echo "VOID: writes through the pnfs mount failed"; exit 1; }
vm "for i in \$(seq 1 $READERS); do cp /var/tmp/dsl-src-solo.bin /srv/dsl-solo/export/$RUNTAG/f\$i.bin; done
    for i in \$(seq 1 $READERS); do cp /var/tmp/dsl-src-knfsd.bin /srv/dsl-knfsd/$RUNTAG/f\$i.bin; done"

# ── guards ────────────────────────────────────────────────────────────
echo "=== GUARDS ==="
TOTAL_BYTES=$((READERS * SIZE_MB * 1048576))
ds_bytes=$(vm "du -sb /srv/dsl-ds1/data | cut -f1" | tr -d '[:space:]')
if [ "${ds_bytes:-0}" -lt $((TOTAL_BYTES * 9 / 10)) ]; then
  echo "VOID: DS holds ${ds_bytes:-0} bytes of $TOTAL_BYTES — stripes did not land on the DS"
  exit 1
fi
echo "  DS holds $ds_bytes bytes — stripes are on the DS"
for m in dsl-pnfs dsl-solo dsl-knfsd; do
  arm=${m#dsl-}
  src=$(vm "md5sum /var/tmp/dsl-src-$arm.bin | cut -d' ' -f1" | tr -d '[:space:]')
  via=$(vm "dd if=/mnt/$m/$RUNTAG/f1.bin bs=1M iflag=direct status=none | md5sum | cut -d' ' -f1" | tr -d '[:space:]')
  [ "$via" = "$src" ] || { echo "VOID: /mnt/$m served bytes that are not the $arm arm's content"; exit 1; }
  echo "  $m: identity + content verified"
done

MDS_PID=$(vm "systemctl show -p MainPID --value flint-dsl-mds" | tr -d '[:space:]')
DS_PID=$(vm "systemctl show -p MainPID --value flint-dsl-ds" | tr -d '[:space:]')
SOLO_PID=$(vm "systemctl show -p MainPID --value flint-dsl-solo" | tr -d '[:space:]')

# Splice execution telemetry (not a gate — the BEFORE binary has no DS
# splice and legitimately shows 0): pipe fds held by each server. A
# post-splice DS serving READs holds pooled pipes; 0 there means the
# staging path never fired and a favourable number would not be
# evidence for it.
for who in mds:$MDS_PID ds:$DS_PID solo:$SOLO_PID; do
  p=${who#*:}
  n=$(vm "ls -l /proc/$p/fd 2>/dev/null | grep -c pipe || true" | tr -d '[:space:]')
  echo "  pipe fds ${who%%:*}: ${n:-?}"
done

# ── measurement ───────────────────────────────────────────────────────
sys_busy() { vm "awk '/^cpu /{idle=\$5+\$6; t=0; for(i=2;i<=NF;i++) t+=\$i; print t-idle}' /proc/stat" | tr -d '[:space:]'; }
proc_ticks() { # $1=pid -> utime+stime ticks (0 if gone)
  vm "awk '{print \$14+\$15}' /proc/$1/stat 2>/dev/null || echo 0" | tr -d '[:space:]'
}

run_arm() { # $1=mountpoint -> "busy_ms wall_ms mds_ticks ds_ticks solo_ticks"
  local m=$1 b0 b1 w0 w1 m0 m1 d0 d1 s0 s1
  b0=$(sys_busy); m0=$(proc_ticks "$MDS_PID"); d0=$(proc_ticks "$DS_PID"); s0=$(proc_ticks "$SOLO_PID")
  w0=$(date +%s%N)
  vm "for p in \$(seq 1 $PASSES); do
        for i in \$(seq 1 $READERS); do
          dd if=$m/f\$i.bin of=/dev/null bs=1M count=$SIZE_MB iflag=direct status=none &
        done; wait
      done" >/dev/null 2>&1
  w1=$(date +%s%N)
  b1=$(sys_busy); m1=$(proc_ticks "$MDS_PID"); d1=$(proc_ticks "$DS_PID"); s1=$(proc_ticks "$SOLO_PID")
  local tck; tck=$(vm "getconf CLK_TCK" | tr -d '[:space:]')
  echo "$(( (b1-b0)*1000/tck )) $(( (w1-w0)/1000000 )) $(( (m1-m0)*1000/tck )) $(( (d1-d0)*1000/tck )) $(( (s1-s0)*1000/tck ))"
}

echo "=== WARM (unscored) ==="
for m in dsl-pnfs dsl-solo dsl-knfsd; do run_arm /mnt/$m/$RUNTAG >/dev/null; done
# The reads have now run once — a spliced READ path holds pooled pipes.
for who in ds:$DS_PID solo:$SOLO_PID; do
  p=${who#*:}
  n=$(vm "ls -l /proc/$p/fd 2>/dev/null | grep -c pipe || true" | tr -d '[:space:]')
  echo "  pipe fds after warm ${who%%:*}: ${n:-?}"
done

echo "=== REPS (interleaved) ==="
CSV=/tmp/dsl.csv
echo "rep,arm,busy_ms,wall_ms,mds_ms,ds_ms,solo_ms,bytes" > "$CSV"
BYTES_PER_REP=$((READERS * SIZE_MB * PASSES * 1048576))
for r in $(seq 1 "$REPS"); do
  for a in dsl-pnfs dsl-solo dsl-knfsd; do
    read -r ms wms mms dms sms <<<"$(run_arm /mnt/$a/$RUNTAG)"
    echo "$r,${a#dsl-},$ms,$wms,$mms,$dms,$sms,$BYTES_PER_REP" >> "$CSV"
    echo "  rep $r ${a#dsl-}: ${ms}ms sys-cpu, ${wms}ms wall (mds ${mms}ms, ds ${dms}ms, solo ${sms}ms)"
    # Execution guard: in the pnfs arm the DS does the byte work. An
    # MDS-served rep (layout refused, silent fallback) is VOID.
    if [ "${a#dsl-}" = pnfs ] && [ "$dms" -le "$mms" ]; then
      echo "VOID: pnfs rep $r — DS cpu ${dms}ms <= MDS cpu ${mms}ms; READs are not flowing through the DS"
      exit 1
    fi
  done
done

echo "=== VM HEALTH AFTER ==="
vm "ps -eo pcpu,comm --sort=-pcpu | head -4"

# ── RPC-count diagnostic (unscored): the runt detector ────────────────
# Fresh DS connection, one 64 MiB file, unmount to flush the DS's
# "closed after ... (N RPCs)" line. Clean 1 MiB READs -> ~64+setup;
# the exact-1MiB fore-channel split -> ~2x that.
echo "=== RPC DIAGNOSTIC ==="
vm "umount -l /mnt/dsl-pnfs && sleep 1 && \
    mount -t nfs -o vers=4.1,port=$MDS_PORT,nolock 127.0.0.1:/ /mnt/dsl-pnfs && \
    dd if=/mnt/dsl-pnfs/$RUNTAG/f1.bin of=/dev/null bs=1M iflag=direct status=none && \
    umount -l /mnt/dsl-pnfs && sleep 1"
vm "journalctl -u flint-dsl-ds --no-pager | grep 'closed after' | tail -3"

echo "=== CSV ==="
cat "$CSV"

# ── summary: medians of per-rep paired ratios ─────────────────────────
echo "=== SUMMARY (medians of per-rep paired ratios; MiB/s from wall) ==="
awk -F, -v bytes="$BYTES_PER_REP" '
  NR>1 { wall[$2","$1]=$4; busy[$2","$1]=$3; reps=($1>reps)?$1:reps }
  END {
    for (r=1; r<=reps; r++) {
      thr["pnfs",r]  = bytes/1048576 / (wall["pnfs,"r]/1000)
      thr["solo",r]  = bytes/1048576 / (wall["solo,"r]/1000)
      thr["knfsd",r] = bytes/1048576 / (wall["knfsd,"r]/1000)
      cpu["pnfs",r]  = busy["pnfs,"r]  / (bytes/1073741824)
      cpu["solo",r]  = busy["solo,"r]  / (bytes/1073741824)
      cpu["knfsd",r] = busy["knfsd,"r] / (bytes/1073741824)
      rt_pk[r]=thr["pnfs",r]/thr["knfsd",r]; rt_sk[r]=thr["solo",r]/thr["knfsd",r]
      rt_ps[r]=thr["pnfs",r]/thr["solo",r]
    }
    n=asort2(rt_pk,a); mpk=a[int((n+1)/2)]
    n=asort2(rt_sk,a); msk=a[int((n+1)/2)]
    n=asort2(rt_ps,a); mps=a[int((n+1)/2)]
    printf "  arm    med MiB/s   med sys-cpu-ms/GiB\n"
    split("pnfs solo knfsd", arms, " ")
    for (i=1;i<=3;i++) {
      m=arms[i]; delete v; for (r=1;r<=reps;r++) v[r]=thr[m,r]
      n=asort2(v,a); tmed=a[int((n+1)/2)]
      delete v; for (r=1;r<=reps;r++) v[r]=cpu[m,r]
      n=asort2(v,a); cmed=a[int((n+1)/2)]
      printf "  %-6s %9.0f   %10.0f\n", m, tmed, cmed
    }
    printf "  ratio pnfs/knfsd: %.3f   solo/knfsd: %.3f   pnfs/solo: %.3f\n", mpk, msk, mps
  }
  function asort2(src, dst,   i, j, t, n) {
    n=0; for (i in src) dst[++n]=src[i]
    for (i=1;i<n;i++) for (j=i+1;j<=n;j++) if (dst[j]<dst[i]) { t=dst[i]; dst[i]=dst[j]; dst[j]=t }
    return n
  }' "$CSV"
