#!/bin/bash
# Does flint's remaining deficit vs knfsd come from WRITER-LOCK CONTENTION
# or from a per-RPC floor?
#
# THE QUESTION. splice-vs-knfsd.sh measured flint-splice at 290 cpu-ms/GiB
# against knfsd's 270 (+7% CPU) but only 86% of knfsd's throughput. Those
# two figures do not track: a pure cost-per-byte deficit would move both
# together. Something is serializing rather than merely costing.
#
# THE SUSPECT. `BackChannelWriter::send_record_segments` holds ONE mutex
# per TCP connection across the marker, every segment, the pre-splice
# flush, `Staged::drain_to`'s `writable().await` readiness loop, and the
# trailing flush. splice-vs-knfsd.sh mounts with NO nconnect, so four
# concurrent readers share one connection and one writer mutex. The unit
# test `a_small_reply_is_head_of_line_blocked_by_a_large_one` proves that
# serialization exists in isolation; this rig asks whether it is what
# costs throughput against knfsd.
#
# THE DISCRIMINATOR. Run each server at nconnect=1 and nconnect=4.
#   * contention    -> flint gains sharply from nc1->nc4; knfsd gains
#                      little, and the flint/knfsd ratio moves toward 1.0.
#   * per-RPC floor -> both gain similarly; the RATIO barely moves.
# The ratio is the score. Absolute cpu-ms/GiB drifts 4-6% between runs
# while ratios hold within 3%.
#
# WHY THIS IS NOT ONLY A DIAGNOSTIC. Production already mounts nconnect=4
# (`pnfs_csi.rs` hardcodes it; every lite/gateway doc calls nconnect>=2
# mandatory). So the nc1 leg is the configuration flint does NOT ship and
# the nc4 leg is the one it does. If the gap closes at nc4, the published
# 86%-of-knfsd figure understates shipped throughput.
#
# ONE CONFIGURATION MOUNTED AT A TIME -- AND THE FIRST DESIGN GOT THIS
# WRONG. nconnect is a property of the client's shared `nfs_client`, which
# the kernel keys on SERVER IDENTITY, not on the address dialled. Mounting
# the same server twice -- even through two different loopback addresses
# -- makes the second mount TRUNK onto the first and silently inherit its
# connection count. The first version of this rig did exactly that and
# GUARD 1 caught it: the nc4 arms held 0 connections of their own. So each
# rep now mounts nc=1, measures, unmounts, then mounts nc=4, measures,
# unmounts. Alternating INSIDE the rep loop keeps the interleaving that
# makes drift hit both legs equally.
set -u
VM=${VM:-flint-nfs-client}
BIN=${BIN:-/tmp/flint-splice-mds}
REPS=${REPS:-4}
READERS=${READERS:-4}
SIZE_MB=${SIZE_MB:-64}
PASSES=${PASSES:-8}
PORT_F=${PORT_F:-20893}   # private: two sessions share this VM
PORT_C=${PORT_C:-20894}   # same server, SPLICE=0 -- the positive control
NCONNS=${NCONNS:-"1 2 4 8"}   # the sweep
CSV=${CSV:-/tmp/nc-contention.csv}
vm() { limactl shell "$VM" sudo bash -c "$1"; }

cleanup() {
  limactl shell "${VM:-flint-nfs-client}" sudo bash -c \
    'umount -l /mnt/ncf 2>/dev/null||true; umount -l /mnt/ncc 2>/dev/null||true; umount -l /mnt/nck 2>/dev/null||true' >/dev/null 2>&1
}
trap cleanup EXIT

echo "=== VM HEALTH BEFORE ==="
vm "uptime; echo vcpus=\$(nproc); vmstat 1 2 | tail -1"

echo "=== SETUP ==="
vm "umount -l /mnt/ncf 2>/dev/null||true; umount -l /mnt/nck 2>/dev/null||true
    systemctl stop flint-nc 2>/dev/null||true
    systemctl stop flint-ncc 2>/dev/null||true
    rm -rf /srv/nc; mkdir -p /srv/nc/export /srv/nc/state /srv/nc/state2 /mnt/ncf /mnt/ncc /mnt/nck
    chmod 0777 /srv/nc/export
    for i in \$(seq 1 $READERS); do
      dd if=/dev/urandom of=/srv/nc/export/f\$i.bin bs=1M count=$SIZE_MB status=none
    done
    echo flint > /srv/nc/export/WHICH_ARM"

vm "cat > /srv/nc/mds.yaml <<CFG
apiVersion: flint.io/v1alpha1
kind: PnfsConfig
mode: standalone
mds:
  bind: { address: \"0.0.0.0\", port: $PORT_F }
  layout: { type: file, stripeSize: 8388608, policy: stripe }
  dataServers: []
  state: { backend: sqlite, config: { path: /srv/nc/state/state.db } }
exports:
  - path: /srv/nc/export
    fsid: 1
    options: [rw, sync, no_subtree_check]
    access: [{ network: 0.0.0.0/0, permissions: rw }]
logging: { level: warn, format: text, components: { mds: warn } }
monitoring:
  prometheus: { enabled: false, port: 0, path: /metrics }
  health: { enabled: false, port: 0, path: /health }
  metrics: []
CFG
    systemd-run --unit=flint-nc --collect --setenv=RUST_LOG=warn \
      --setenv=FLINT_NFS_SPLICE=1 $BIN --config /srv/nc/mds.yaml >/dev/null 2>&1"
sleep 4
vm "systemctl is-active --quiet flint-nc" || { echo "VOID: flint-nc did not start"; vm "journalctl -u flint-nc -n 20 --no-pager"; exit 1; }

# POSITIVE CONTROL. Same binary, same export, same workload -- only
# FLINT_NFS_SPLICE differs. splice is known to be worth ~1.5x throughput
# here, so if this rig cannot see THAT, it cannot be trusted to report
# "nconnect changed nothing" either. A null result without this leg is
# indistinguishable from a rig that resolves nothing at all.
vm "sed -e 's#port: $PORT_F#port: $PORT_C#' -e 's#/srv/nc/state/state.db#/srv/nc/state2/state.db#' \
      /srv/nc/mds.yaml > /srv/nc/mds-copy.yaml
    systemd-run --unit=flint-ncc --collect --setenv=RUST_LOG=warn \
      --setenv=FLINT_NFS_SPLICE=0 $BIN --config /srv/nc/mds-copy.yaml >/dev/null 2>&1"
sleep 4
vm "systemctl is-active --quiet flint-ncc" || { echo "VOID: flint-ncc did not start"; vm "journalctl -u flint-ncc -n 20 --no-pager"; exit 1; }

vm "rm -rf /srv/nc-knfsd; mkdir -p /srv/nc-knfsd; chmod 0777 /srv/nc-knfsd
    for i in \$(seq 1 $READERS); do
      dd if=/dev/urandom of=/srv/nc-knfsd/f\$i.bin bs=1M count=$SIZE_MB status=none
    done
    echo knfsd > /srv/nc-knfsd/WHICH_ARM
    grep -q /srv/nc-knfsd /etc/exports 2>/dev/null || \
      echo '/srv/nc-knfsd 127.0.0.0/8(rw,sync,no_subtree_check,no_root_squash,fsid=11)' >> /etc/exports
    exportfs -ra; systemctl restart nfs-kernel-server; sleep 3"

mount_at() { # nconnect
  local nc=$1
  vm "mount -t nfs -o vers=4.1,port=$PORT_F,nolock,nconnect=$nc 127.0.0.1:/ /mnt/ncf
      mount -t nfs -o vers=4.1,port=$PORT_C,nolock,nconnect=$nc 127.0.0.1:/ /mnt/ncc
      mount -t nfs -o vers=4.1,nolock,nconnect=$nc 127.0.0.1:/srv/nc-knfsd /mnt/nck" >/dev/null 2>&1
  vm "mountpoint -q /mnt/ncf && mountpoint -q /mnt/ncc && mountpoint -q /mnt/nck" || { echo "VOID: mount failed at nconnect=$nc"; exit 1; }
}
umount_all() { vm "umount -l /mnt/ncf 2>/dev/null||true; umount -l /mnt/ncc 2>/dev/null||true; umount -l /mnt/nck 2>/dev/null||true; sleep 1" >/dev/null 2>&1; }

# GUARD 1 -- CONNECTION COUNT, gated on flint only.
#
# The entire experiment is the difference between 1 and 4 connections. If
# nconnect were silently ignored (the trunking trap above), nc1 and nc4
# would be the SAME configuration, and "no difference" would be
# indistinguishable from "contention is not the cause" -- precisely the
# conclusion this rig exists to test. Checked on EVERY mount.
#
# KNFSD CANNOT BE CONTROLLED HERE AND IS NOT GATED. knfsd is a kernel
# singleton on 2049 shared by everything on this VM, and a second session
# already holds a mount against it. A new mount TRUNKS onto that existing
# nfs_client and inherits its connection count no matter what this rig
# asks for -- observed directly: nconnect=1 produced 4 connections. flint
# is safe because it runs on a private port that only this rig mounts.
#
# So knfsd's count is REPORTED, not asserted, and it is a fixed reference
# arm rather than a second variable. The flint nc1->nc4 delta is the
# primary result and needs no knfsd control to be read; what is lost is
# the ability to say "the workload can resolve connection count" from an
# independent server, so a NULL flint result here is weaker evidence than
# a positive one. Re-run on an unshared VM to recover that.
KNFSD_CONNS=0
check_conns() { # nconnect
  local want=$1 f
  f=$(vm "ss -tn state established '( dport = :$PORT_F )' | grep -c ':$PORT_F'" | tr -d '[:space:]')
  KNFSD_CONNS=$(vm "ss -tn state established '( dport = :2049 )' | grep -c ':2049'" | tr -d '[:space:]')
  echo "  connections: flint=$f (want $want)  knfsd=$KNFSD_CONNS (uncontrolled, reference)"
  [ "$f" = "$want" ] || {
    echo "  *** VOID: flint nconnect=$want did not take effect (got $f)"; return 1; }
  return 0
}

echo "=== GUARDS at nconnect=1 ==="
mount_at 1
check_conns 1 || exit 1
for m in ncf ncc nck; do
  who=$(vm "cat /mnt/$m/WHICH_ARM 2>/dev/null" | tr -d '[:space:]')
  exp=flint; [ "$m" = nck ] && exp=knfsd
  [ "$who" = "$exp" ] || { echo "VOID: /mnt/$m serving '$who', expected '$exp'"; exit 1; }
  src=$(vm "md5sum $( [ "$m" = nck ] && echo /srv/nc-knfsd || echo /srv/nc/export )/f1.bin | cut -d' ' -f1" | tr -d '[:space:]')
  via=$(vm "dd if=/mnt/$m/f1.bin bs=1M iflag=direct status=none | md5sum | cut -d' ' -f1" | tr -d '[:space:]')
  [ "$src" = "$via" ] || { echo "VOID: $m served bytes differing from disk"; exit 1; }
  echo "  $m: $who, content matches"
done
# GUARD 3 -- splice must actually have fired, or this measures the copy path.
P=$(vm "ls -l /proc/\$(systemctl show -p MainPID --value flint-nc)/fd 2>/dev/null | grep -c pipe || true" | tr -d '[:space:]')
PC=$(vm "ls -l /proc/\$(systemctl show -p MainPID --value flint-ncc)/fd 2>/dev/null | grep -c pipe || true" | tr -d '[:space:]')
echo "  pipe fds: splice-arm=$P  copy-arm=$PC"
[ "${P:-0}" -gt 0 ] || { echo "VOID: no pipes on the splice arm -- splice never fired"; exit 1; }
[ "${PC:-0}" -eq 0 ] || { echo "VOID: the copy arm holds pipes -- it is not the copy path"; exit 1; }
umount_all

sys_busy() { vm "awk '/^cpu /{idle=\$5+\$6; t=0; for(i=2;i<=NF;i++) t+=\$i; print t-idle}' /proc/stat" | tr -d '[:space:]'; }

run_arm() { # mountpoint -> "busy_ms wall_ms bytes"
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

echo "=== WARM (measured reps are page-cache warm; O_DIRECT on the client) ==="
mount_at 4; for m in ncf ncc nck; do run_arm "/mnt/$m" >/dev/null; done; umount_all

echo "=== REPS (nc=1 and nc=4 alternate INSIDE each rep) ==="
echo "rep,nconnect,arm,busy_ms,wall_ms,bytes" > "$CSV"
for r in $(seq 1 "$REPS"); do
  for nc in $NCONNS; do
    mount_at "$nc"
    check_conns "$nc" || { echo "VOID mid-run at rep $r nc=$nc"; exit 1; }
    for m in ncf ncc nck; do
      arm=flint; [ "$m" = nck ] && arm=knfsd; [ "$m" = ncc ] && arm=flintcopy
      read -r ms wms by <<<"$(run_arm "/mnt/$m")"
      echo "$r,$nc,$arm,$ms,$wms,$by" >> "$CSV"
      echo "    rep $r nc=$nc $arm: ${ms}ms cpu, ${wms}ms wall, $((by/1048576))MiB"
    done
    umount_all
  done
done

echo "=== VM HEALTH AFTER (a busy VM compresses every ratio toward 1.0) ==="
vm "vmstat 1 2 | tail -1"

echo "=== RESULT ==="
python3 - "$CSV" <<'PY'
import csv, sys, statistics as st
rows=list(csv.DictReader(open(sys.argv[1])))
ncs=sorted({int(r['nconnect']) for r in rows})
def sel(nc,arm,f): return [float(r[f]) for r in rows if int(r['nconnect'])==nc and r['arm']==arm]
def gib(nc,arm):
    b=sel(nc,arm,'bytes'); return b[0]/1073741824 if b else float('nan')
print(f"{'nconn':>6}{'flint MiB/s':>13}{'cpu-ms/GiB':>12}{'vs nc=1':>9}{'knfsd MiB/s':>13}{'copy MiB/s':>12}")
base=None; out={}
for nc in ncs:
    g=gib(nc,'flint'); w=st.median(sel(nc,'flint','wall_ms')); c=st.median(sel(nc,'flint','busy_ms'))
    mibs=g*1024/(w/1000); cpu=c/g
    if base is None: base=mibs
    kw=sel(nc,'knfsd','wall_ms'); km=gib(nc,'knfsd')*1024/(st.median(kw)/1000) if kw else float('nan')
    cw=sel(nc,'flintcopy','wall_ms'); cm=gib(nc,'flintcopy')*1024/(st.median(cw)/1000) if cw else float('nan')
    out[nc]=(mibs,cpu,km,cm)
    print(f"{nc:>6}{mibs:13.0f}{cpu:12.1f}{mibs/base:9.3f}{km:13.0f}{cm:12.0f}")
print()
span=max(out[n][0] for n in ncs)/min(out[n][0] for n in ncs)
kspan=max(out[n][2] for n in ncs)/min(out[n][2] for n in ncs)
print(f"flint throughput span across nconnect: {span:.3f}")
print(f"knfsd span (same config every leg -> this is DRIFT): {kspan:.3f}")
ctl=out[ncs[-1]][0]/out[ncs[-1]][3]
print(f"POSITIVE CONTROL splice/copy at nc={ncs[-1]}: {ctl:.3f}")
print()
if ctl < 1.10:
    print("=> VOID-ISH: the rig cannot resolve even splice-vs-copy, which is")
    print("   known to be large here. Nothing about nconnect can be trusted.")
elif span <= kspan or span < 1.05:
    print("=> NO SCALING. flint does not gain from more connections, and the")
    print("   spread is no larger than knfsd's drift across identical legs.")
    print("   The per-connection writer mutex is not what caps throughput.")
else:
    print(f"=> SCALES. flint gains {(span-1)*100:.0f}% across the sweep, beyond knfsd's")
    print( "   drift, so connection count IS a lever and the writer mutex was")
    print( "   capping throughput at low connection counts.")
PY
echo "=== CSV ==="; cat "$CSV"
