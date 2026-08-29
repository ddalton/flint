#!/bin/bash
# musl mallocng vs mimalloc on the NFS read path.
#
# WHY. flint ships as a STATIC MUSL binary with no custom allocator. A
# read-path profile (strace -c, 4 readers, O_DIRECT, splice ON) found
# futex at 66% of syscall time and ~2.4 mmap+munmap PER READ in 16-28 KiB
# chunks. The payload is spliced and never allocated, so that churn is the
# allocator, not the data. musl's mallocng is known to serialise under
# multithreaded load, and it is PROCESS-GLOBAL -- which is also why
# splice-nconnect-contention.sh found that adding TCP connections changed
# nothing (nc4/nc1 = 0.989 against a control that resolved 1.478).
#
# Two binaries from ONE source tree, differing in the `fastalloc` feature
# and nothing else. Both run splice ON, both at nconnect=4 (what
# production mounts), interleaved per rep.
#
# SCORED AS cpu-ms/GiB. Absolute figures drift 4-6% between runs; ratios
# hold within 3%. knfsd is a fixed reference arm -- its connection count
# cannot be controlled on a shared VM (see splice-nconnect-contention.sh).
set -u
VM=${VM:-flint-nfs-client}
BIN_BASE=${BIN_BASE:-/tmp/mds-base}
BIN_MI=${BIN_MI:-/tmp/mds-mi}
REPS=${REPS:-5}
READERS=${READERS:-4}
SIZE_MB=${SIZE_MB:-64}
PASSES=${PASSES:-8}
PORT_B=${PORT_B:-20895}
PORT_M=${PORT_M:-20896}
CSV=${CSV:-/tmp/alloc-diff.csv}
vm() { limactl shell "$VM" sudo bash -c "$1"; }

cleanup() {
  limactl shell "${VM:-flint-nfs-client}" sudo bash -c \
    'umount -l /mnt/ab 2>/dev/null||true; umount -l /mnt/am 2>/dev/null||true; umount -l /mnt/ak 2>/dev/null||true
     systemctl stop flint-ab flint-am 2>/dev/null||true' >/dev/null 2>&1
}
trap cleanup EXIT

echo "=== VM HEALTH BEFORE ==="; vm "uptime; echo vcpus=\$(nproc); vmstat 1 2 | tail -1"

# ALLOCATOR GUARD. If the feature silently failed to apply, both arms
# would be the same binary and "no difference" would be vacuous -- the
# exact reading this rig exists to produce or refute.
echo "=== GUARD: the two binaries really differ in allocator ==="
for pair in "base:$BIN_BASE:0" "mi:$BIN_MI:1"; do
  # ${pair#*:} keeps the trailing ":want" -- strip it first, or `strings`
  # gets a path that does not exist, returns 0 for BOTH arms, and the
  # guard reads as "the feature did not apply" no matter what is true.
  mid=${pair%:*}; mid=${mid#*:}
  n=$(vm "grep -ac mimalloc $mid || true" | tr -d '[:space:]')
  want=${pair##*:}; nm=${pair%%:*}
  echo "  $nm: mimalloc symbols=$n"
  if [ "$want" = 0 ]; then [ "${n:-0}" -eq 0 ] || { echo "  *** VOID: baseline carries mimalloc"; exit 1; }
  else [ "${n:-0}" -gt 0 ] || { echo "  *** VOID: fastalloc arm has NO mimalloc -- the feature did not apply"; exit 1; }; fi
done

setup() { # unit port binary
  local u=$1 p=$2 b=$3
  vm "umount -l /mnt/$u 2>/dev/null||true; systemctl stop flint-$u 2>/dev/null||true
      rm -rf /srv/$u; mkdir -p /srv/$u/export /srv/$u/state /mnt/$u; chmod 0777 /srv/$u/export
      for i in \$(seq 1 $READERS); do dd if=/dev/urandom of=/srv/$u/export/f\$i.bin bs=1M count=$SIZE_MB status=none; done
      echo $u > /srv/$u/export/WHICH_ARM
      cat > /srv/$u/mds.yaml <<CFG
apiVersion: flint.io/v1alpha1
kind: PnfsConfig
mode: standalone
mds:
  bind: { address: \"0.0.0.0\", port: $p }
  layout: { type: file, stripeSize: 8388608, policy: stripe }
  dataServers: []
  state: { backend: sqlite, config: { path: /srv/$u/state/state.db } }
exports:
  - path: /srv/$u/export
    fsid: 1
    options: [rw, sync, no_subtree_check]
    access: [{ network: 0.0.0.0/0, permissions: rw }]
logging: { level: warn, format: text, components: { mds: warn } }
monitoring:
  prometheus: { enabled: false, port: 0, path: /metrics }
  health: { enabled: false, port: 0, path: /health }
  metrics: []
CFG
      systemd-run --unit=flint-$u --collect --setenv=RUST_LOG=warn \
        --setenv=FLINT_NFS_SPLICE=1 $b --config /srv/$u/mds.yaml >/dev/null 2>&1"
  sleep 4
  vm "systemctl is-active --quiet flint-$u" || { echo "VOID: flint-$u did not start"; vm "journalctl -u flint-$u -n 20 --no-pager"; exit 1; }
  vm "mount -t nfs -o vers=4.1,port=$p,nolock,nconnect=4 127.0.0.1:/ /mnt/$u" >/dev/null 2>&1
  vm "mountpoint -q /mnt/$u" || { echo "VOID: /mnt/$u did not mount"; exit 1; }
}

echo "=== SETUP ==="
setup ab "$PORT_B" "$BIN_BASE"
setup am "$PORT_M" "$BIN_MI"
vm "mkdir -p /mnt/ak; grep -q /srv/nc-knfsd /etc/exports 2>/dev/null || \
      echo '/srv/nc-knfsd 127.0.0.0/8(rw,sync,no_subtree_check,no_root_squash,fsid=11)' >> /etc/exports
    mkdir -p /srv/nc-knfsd; chmod 0777 /srv/nc-knfsd
    for i in \$(seq 1 $READERS); do
      [ -f /srv/nc-knfsd/f\$i.bin ] || dd if=/dev/urandom of=/srv/nc-knfsd/f\$i.bin bs=1M count=$SIZE_MB status=none; done
    echo ak > /srv/nc-knfsd/WHICH_ARM
    exportfs -ra; mount -t nfs -o vers=4.1,nolock 127.0.0.1:/srv/nc-knfsd /mnt/ak" >/dev/null 2>&1

echo "=== GUARDS: identity, content, splice ==="
for a in ab am ak; do
  who=$(vm "cat /mnt/$a/WHICH_ARM 2>/dev/null" | tr -d '[:space:]')
  [ "$who" = "$a" ] || { echo "VOID: /mnt/$a serving '$who'"; exit 1; }
  srcdir=/srv/$a/export; [ "$a" = ak ] && srcdir=/srv/nc-knfsd
  src=$(vm "md5sum $srcdir/f1.bin | cut -d' ' -f1" | tr -d '[:space:]')
  via=$(vm "dd if=/mnt/$a/f1.bin bs=1M iflag=direct status=none | md5sum | cut -d' ' -f1" | tr -d '[:space:]')
  [ "$src" = "$via" ] || { echo "VOID: $a served bytes differing from disk"; exit 1; }
  echo "  $a: identity + content OK"
done
for a in ab am; do
  n=$(vm "ls -l /proc/\$(systemctl show -p MainPID --value flint-$a)/fd 2>/dev/null | grep -c pipe || true" | tr -d '[:space:]')
  echo "  $a pipe fds: $n"
  [ "${n:-0}" -gt 0 ] || { echo "VOID: splice never fired on $a"; exit 1; }
done

sys_busy() { vm "awk '/^cpu /{idle=\$5+\$6; t=0; for(i=2;i<=NF;i++) t+=\$i; print t-idle}' /proc/stat" | tr -d '[:space:]'; }
run_arm() {
  local m=$1 b0 b1 w0 w1 tck
  b0=$(sys_busy); w0=$(date +%s%N)
  vm "for p in \$(seq 1 $PASSES); do
        for i in \$(seq 1 $READERS); do
          dd if=$m/f\$i.bin of=/dev/null bs=1M count=$SIZE_MB iflag=direct status=none &
        done; wait
      done" >/dev/null 2>&1
  w1=$(date +%s%N); b1=$(sys_busy)
  tck=$(vm "getconf CLK_TCK" | tr -d '[:space:]')
  echo "$(( (b1 - b0) * 1000 / tck )) $(( (w1 - w0) / 1000000 )) $(( READERS * SIZE_MB * PASSES * 1048576 ))"
}

echo "=== WARM ==="; for a in ab am ak; do run_arm "/mnt/$a" >/dev/null; done
echo "=== REPS (interleaved) ==="
echo "rep,arm,busy_ms,wall_ms,bytes" > "$CSV"
for r in $(seq 1 "$REPS"); do
  for a in ab am ak; do
    read -r ms wms by <<<"$(run_arm "/mnt/$a")"
    echo "$r,$a,$ms,$wms,$by" >> "$CSV"
    echo "  rep $r $a: ${ms}ms cpu, ${wms}ms wall"
  done
done
echo "=== VM HEALTH AFTER ==="; vm "vmstat 1 2 | tail -1"

echo "=== SYSCALL PROFILE: mmap churn is the mechanism under test ==="
for a in ab am; do
  PID=$(vm "systemctl show -p MainPID --value flint-$a" | tr -d '[:space:]')
  vm "timeout 8 strace -f -c -p $PID > /tmp/prof-$a.txt 2>&1 &
      sleep 1
      for i in 1 2 3 4; do dd if=/mnt/$a/f\$i.bin of=/dev/null bs=1M count=48 iflag=direct status=none & done; wait
      sleep 1" >/dev/null 2>&1
  echo "--- $a ---"
  vm "grep -E ' (mmap|munmap|futex|splice|sendto)$' /tmp/prof-$a.txt | awk '{printf \"    %-10s calls=%-7s %s%%\n\", \$NF, \$4, \$1}'"
done

echo "=== RESULT ==="
python3 - "$CSV" <<'PY'
import csv, sys, statistics as st
rows=list(csv.DictReader(open(sys.argv[1])))
def med(a,f): return st.median([float(r[f]) for r in rows if r['arm']==a])
def gib(a): return [float(r['bytes']) for r in rows if r['arm']==a][0]/1073741824
name={'ab':'musl malloc','am':'mimalloc','ak':'knfsd'}
res={}
print(f"{'arm':14}{'cpu-ms/GiB':>12}{'MiB/s':>9}")
for a in ('ab','am','ak'):
    g=gib(a); cpu=med(a,'busy_ms')/g; mibs=g*1024/(med(a,'wall_ms')/1000)
    res[a]=(cpu,mibs); print(f"{name[a]:14}{cpu:12.1f}{mibs:9.0f}")
print()
cpu_r=res['am'][0]/res['ab'][0]; tp_r=res['am'][1]/res['ab'][1]
print(f"mimalloc/musl  CPU per byte: {cpu_r:.3f}  (lower is better)")
print(f"mimalloc/musl  throughput:   {tp_r:.3f}  (higher is better)")
print(f"vs knfsd  musl: {res['ab'][1]/res['ak'][1]:.3f}   mimalloc: {res['am'][1]/res['ak'][1]:.3f}")
print()
if tp_r > 1.05 or cpu_r < 0.95:
    print("=> THE ALLOCATOR IS A REAL COST. Swapping it is a contained change")
    print("   (one dependency + one global_allocator line, no logic touched).")
elif tp_r < 0.95 or cpu_r > 1.05:
    print("=> mimalloc is WORSE here. Keep musl's allocator; the per-RPC cost")
    print("   is elsewhere.")
else:
    print("=> NO MEANINGFUL DIFFERENCE. The mmap/futex churn is not costing")
    print("   throughput, so the allocator is not the lever. Do NOT take the")
    print("   dependency.")
PY
echo "=== CSV ==="; cat "$CSV"
