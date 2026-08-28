#!/usr/bin/env bash
# Leg L-perf — throughput and metadata rate against flint, WITH a knfsd
# control arm. The repo's first performance gate of any kind.
#
# WHY THERE WAS NONE, AND WHY THAT IS THE PROBLEM. Nothing in this repo
# measures whether a change made the server slower. The only perf script
# present (scripts/benchmark-nfs-comparison.sh) compares two flint
# binaries against each other, on macOS, using flint-nfs-server — which
# breaks §0 rule 1 (the product binary is flint-pnfs-mds) and §0 rule 2
# (macOS numbers are rig-confounded). So every change to the hot path,
# including a filehandle MAC and a containment check that landed this
# week, is unmeasured.
#
# WHY A DIFFERENTIAL AND NOT A NUMBER. This VM is 2 vCPU / 2 GiB and the
# host is usually compiling something. Absolute MiB/s from it is worth
# nothing: a prior campaign measured the same rig drifting ~2x WITHIN a
# session, which makes any cross-time comparison of absolute numbers void.
# A ratio against knfsd, measured in the same session on the same kernel
# and the same disk, is the only quotable quantity — the rig cancels.
#
# WHY THE ARMS INTERLEAVE. Running all of flint's reps and then all of
# knfsd's would let a drift BETWEEN the two blocks masquerade as a
# difference between the two servers. Interleaving per rep makes drift
# common-mode, which is the whole point of having a control.
#
# WHY THREE DIMENSIONS. One throughput number hides a metadata
# regression, and the agent-fleet workload is metadata-bound, not
# bandwidth-bound. A change that halves the create rate while leaving
# streaming reads untouched must be visible.
#
# ⚠ THIS SCRIPT DOES NOT DECIDE PASS/FAIL. It emits measurements;
# scripts/check-perf.py gates them against a committed baseline. Same
# split as the pjdfstest and xfstests differentials, and for the same
# reason: a harness that scores itself is a harness that can be argued
# with.
set -uo pipefail

VM=${LIMA_VM:-flint-nfs-client}
PORT=${NFS_PORT:-20493}
REPS=${REPS:-5}
SIZE_MB=${SIZE_MB:-128}
META_N=${META_N:-2000}
OUT=${OUT:-tests/lima/perf-latest.json}

MDS_BIN=${MDS_BIN:-/tmp/flint-pnfs-mds-vm}
MDS_CONFIG=${MDS_CONFIG:-/tmp/perf-mds.yaml}
MDS_EXPORT=${MDS_EXPORT:-/srv/flint-perf-export}
MDS_STATE=${MDS_STATE:-/srv/flint-perf-state}
FLINT_MNT=/mnt/perf-flint
KNFSD_MNT=/mnt/perf-knfsd

vm() { limactl shell "$VM" -- sudo bash -lc "$1"; }

trap 'vm "systemctl stop flint-perf 2>/dev/null||true
          umount -f '"$FLINT_MNT"' 2>/dev/null||true
          umount -f '"$KNFSD_MNT"' 2>/dev/null||true" >/dev/null 2>&1 || true' EXIT

echo "── bringing up arm A: flint on port $PORT ──"
vm "systemctl stop flint-perf 2>/dev/null||true; systemctl reset-failed flint-perf 2>/dev/null||true
    umount -f $FLINT_MNT 2>/dev/null||true
    rm -rf $MDS_EXPORT $MDS_STATE; mkdir -p $MDS_EXPORT $MDS_STATE $FLINT_MNT
    chmod 0777 $MDS_EXPORT
    cat > $MDS_CONFIG <<CFG
apiVersion: flint.io/v1alpha1
kind: PnfsConfig
mode: standalone

mds:
  bind:
    address: \"0.0.0.0\"
    port: $PORT

  layout:
    type: file
    stripeSize: 8388608
    policy: stripe

  dataServers: []

  # sqlite, NOT memory. The hub ships sqlite, and the meta dimension
  # below is exactly where the state backend costs something — gating
  # a create/stat rate measured against an in-process map would gate a
  # path nobody runs.
  state:
    backend: sqlite
    config:
      path: $MDS_STATE/state.db

exports:
  - path: $MDS_EXPORT
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
  prometheus:
    enabled: false
    port: 0
    path: /metrics
  health:
    enabled: false
    port: 0
    path: /health
  metrics: []
CFG
    chmod +x $MDS_BIN
    systemd-run --unit=flint-perf --collect --setenv=RUST_LOG=warn \
      $MDS_BIN --config $MDS_CONFIG >/dev/null 2>&1
    sleep 4
    mount -t nfs -o vers=4.1,port=$PORT,nolock 127.0.0.1:/ $FLINT_MNT" >/dev/null 2>&1

# The config schema is not obvious and a wrong one is SILENT: PnfsConfig
# does not deny unknown fields, so a config with invented keys parses
# into an all-defaults standalone server on the wrong port. The mount
# then fails and gate 1 below voids the run — correct, but it reports
# "not nfs" rather than "your config was ignored". Say it plainly here.
if ! vm "systemctl is-active --quiet flint-perf"; then
  echo "FAIL: the flint MDS unit is not active — it died at startup."
  vm "systemctl status flint-perf --no-pager -l 2>&1 | tail -20" || true
  exit 1
fi

echo "── bringing up arm B (control): knfsd ──"
vm "umount -f $KNFSD_MNT 2>/dev/null||true
    rm -rf /srv/knfsd-perf; mkdir -p /srv/knfsd-perf $KNFSD_MNT; chmod 0777 /srv/knfsd-perf
    grep -q /srv/knfsd-perf /etc/exports 2>/dev/null || \
      echo '/srv/knfsd-perf 127.0.0.1/32(rw,sync,no_subtree_check,no_root_squash,fsid=8)' >> /etc/exports
    exportfs -ra; systemctl restart nfs-kernel-server; sleep 3
    mount -t nfs -o vers=4.1,nolock 127.0.0.1:/srv/knfsd-perf $KNFSD_MNT" >/dev/null 2>&1

# ── ANTI-VACUITY GATE 1: both arms must actually be NFS ──────────────
# §0 rule 4. An unmounted target leaves dd writing to the VM's local
# ext4, which produces a magnificent number for whichever arm failed to
# mount — and the differential then reports that as a win.
echo "── asserting both mounts are NFS ──"
for m in $FLINT_MNT $KNFSD_MNT; do
  t=$(vm "findmnt -n -o FSTYPE -T $m 2>/dev/null" | tr -d '[:space:]')
  case "$t" in
    nfs4|nfs) echo "   $m: $t" ;;
    *) echo "VOID: $m is fstype '$t', not nfs — this run would have measured local disk"; exit 1 ;;
  esac
done

# One timed unit of work. Echoes a JSON line per measurement.
#   $1 dimension  $2 arm label  $3 mount  $4 rep
measure() {
  local dim=$1 arm=$2 mnt=$3 rep=$4
  case "$dim" in
    write)
      vm "rm -f $mnt/perf.bin; sync
          s=\$(date +%s%N)
          dd if=/dev/zero of=$mnt/perf.bin bs=1M count=$SIZE_MB conv=fdatasync 2>/dev/null
          e=\$(date +%s%N)
          # The bytes are the anti-vacuity check: a dd that wrote nothing
          # is instantaneous and would otherwise score infinitely well.
          b=\$(stat -c %s $mnt/perf.bin 2>/dev/null || echo 0)
          echo \"{\\\"dim\\\":\\\"write\\\",\\\"arm\\\":\\\"$arm\\\",\\\"rep\\\":$rep,\\\"ns\\\":\$((e-s)),\\\"bytes\\\":\$b}\""
      ;;
    read)
      vm "sync; echo 3 > /proc/sys/vm/drop_caches
          s=\$(date +%s%N)
          b=\$(dd if=$mnt/perf.bin of=/dev/null bs=1M 2>&1 | grep -o '^[0-9]* bytes' | cut -d' ' -f1)
          e=\$(date +%s%N)
          echo \"{\\\"dim\\\":\\\"read\\\",\\\"arm\\\":\\\"$arm\\\",\\\"rep\\\":$rep,\\\"ns\\\":\$((e-s)),\\\"bytes\\\":\${b:-0}}\""
      ;;
    meta)
      vm "rm -rf $mnt/meta; mkdir -p $mnt/meta; cd $mnt/meta
          s=\$(date +%s%N)
          for i in \$(seq 1 $META_N); do : > f\$i; done
          for i in \$(seq 1 $META_N); do stat -c %s f\$i >/dev/null; done
          e=\$(date +%s%N)
          # Counted from a fresh listing, not from the loop variable: a
          # create loop that silently failed still finishes.
          n=\$(ls -1 | wc -l)
          rm -rf $mnt/meta
          echo \"{\\\"dim\\\":\\\"meta\\\",\\\"arm\\\":\\\"$arm\\\",\\\"rep\\\":$rep,\\\"ns\\\":\$((e-s)),\\\"ops\\\":\$n}\""
      ;;
  esac
}

: > "$OUT"
echo "── measuring: $REPS reps x 3 dimensions, arms INTERLEAVED ──"
for rep in $(seq 1 "$REPS"); do
  for dim in write read meta; do
    # flint then knfsd, back to back, inside one rep. Drift between
    # reps is then common to both arms and divides out.
    measure "$dim" flint "$FLINT_MNT" "$rep" >> "$OUT"
    measure "$dim" knfsd "$KNFSD_MNT" "$rep" >> "$OUT"
  done
  echo "   rep $rep/$REPS done"
done

# ── ANTI-VACUITY GATE 2: the checker must be able to FAIL ────────────
# A crippled mount (4 KiB rsize/wsize against a 1 MiB default) is
# provably slower. If the checker cannot see THAT, it cannot see a
# regression either, and every green run above means nothing. This is
# the falsifiability arm, and it is not optional.
echo "── falsifiability: re-measuring flint on a deliberately crippled mount ──"
vm "umount -f $FLINT_MNT 2>/dev/null||true
    mount -t nfs -o vers=4.1,port=$PORT,nolock,rsize=4096,wsize=4096 127.0.0.1:/ $FLINT_MNT" >/dev/null 2>&1
t=$(vm "findmnt -n -o FSTYPE -T $FLINT_MNT 2>/dev/null" | tr -d '[:space:]')
case "$t" in nfs4|nfs) ;; *) echo "VOID: crippled remount is '$t', not nfs"; exit 1;; esac
: > "${OUT%.json}-crippled.json"
for rep in $(seq 1 "$REPS"); do
  for dim in write read meta; do
    measure "$dim" flint "$FLINT_MNT" "$rep" >> "${OUT%.json}-crippled.json"
    measure "$dim" knfsd "$KNFSD_MNT" "$rep" >> "${OUT%.json}-crippled.json"
  done
done

echo
echo "measurements: $OUT"
echo "falsifiability arm: ${OUT%.json}-crippled.json"
echo
echo "Gate them with:"
echo "  python3 scripts/check-perf.py $OUT tests/lima/perf-baseline.json"
echo "  python3 scripts/check-perf.py ${OUT%.json}-crippled.json tests/lima/perf-baseline.json  # MUST fail"
