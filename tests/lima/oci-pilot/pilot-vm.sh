#!/bin/bash
# rung-1 pilot (oci-image-serving-design.md §9.3) — VM-side runner.
# Runs INSIDE the lima VM. Driven by pilot-host.sh; do not edit while running.
#
# Arms per rep, interleaved (§9.3):
#   P1  baseline pull+start: tar.gz over the flint mount + gunzip+untar + first-exec
#   P2  EROFS blob on LOCAL disk, loop-mounted — the format win
#   P3  same blob on the flint RWX mount — the remote-fault tax
#
# SHARED-VM DISCIPLINE (another session owns ports 2049/20490/20491 and its own
# mounts): private port; all state under $BASE; per-mount /proc/self/mountstats
# (never global nfsstat); per-file fadvise eviction + remount (never global
# drop_caches); processes killed by PIDFILE only; loadavg recorded per rep.
set -u

BASE=/var/tmp/oci-pilot
PORT=22049
VOLID=oci-pilot
EXPORT=$BASE/export
LOCAL=$BASE/local
MNT=$BASE/mnt
EMNT_P2=$BASE/emnt-p2
EMNT_P3=$BASE/emnt-p3
P1ROOT=$LOCAL/p1-rootfs
PIDFILE=$BASE/server.pid
SRVLOG=$BASE/server.log
REPS=${REPS:-5}
# first-exec workload: interpreter + libpython + stdlib .so/.pyc = the
# serialized critical-path faults. Output is part of the G4 identity check.
PYEXEC='import json,ssl,sqlite3,decimal,email,http.client,hashlib;print("READY",hashlib.sha256(b"oci-pilot").hexdigest()[:16])'

log() { echo "[pilot-vm] $*" >&2; }
bad() { echo "[pilot-vm] FAIL: $*" >&2; FAILS="$FAILS|$*"; }
FAILS=""

now_ns() { date +%s%N; }

evict_file() { # per-file page-cache eviction — never global drop_caches (shared VM)
  python3 - "$1" <<'PYEOF'
import os, sys
fd = os.open(sys.argv[1], os.O_RDONLY)
os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
os.close(fd)
PYEOF
}

evict_tree() { find "$1" -type f -print0 | while IFS= read -r -d '' f; do evict_file "$f"; done; }

mount_read_stats() { # per-mount READ ops + bytes_recv from /proc/self/mountstats
  awk -v mp="$MNT" '
    $1=="device" { inblk = (index($0, " mounted on " mp " ") > 0) }
    inblk && $1=="READ:" { print $2, $6; found=1 }
    END { if (!found) print 0, 0 }' /proc/self/mountstats
}

server_cpu_ms() { # utime+stime of the flint server, in ms (CLK_TCK=100)
  local pid; pid=$(cat "$PIDFILE" 2>/dev/null) || { echo 0; return; }
  awk '{print int(($14+$15)*10)}' "/proc/$pid/stat" 2>/dev/null || echo 0
}

remount_nfs() {
  sudo umount "$MNT" 2>/dev/null
  sudo mount -t nfs4 -o vers=4.2,sec=sys,port=$PORT,soft,timeo=100 127.0.0.1:/ "$MNT" || return 1
}

loop_mount() { # $1=blob $2=mountpoint ; echoes loop dev
  local dev
  dev=$(sudo losetup --find --show "$1") || return 1
  sudo mount -t erofs -o ro "$dev" "$2" || { sudo losetup -d "$dev"; return 1; }
  echo "$dev"
}

loop_umount() { # $1=mountpoint $2=loopdev
  sudo umount "$1" 2>/dev/null
  [ -n "${2:-}" ] && sudo losetup -d "$2" 2>/dev/null
}

first_exec() { # $1=rootfs ; prints exec output; caller times it
  sudo chroot "$1" /usr/local/bin/python3 -c "$PYEXEC"
}

disk_guard() {
  local free_mb
  free_mb=$(df -m "$BASE" | awk 'NR==2{print $4}')
  [ "$free_mb" -lt 500 ] && { bad "disk guard: ${free_mb}MB free < 500MB"; return 1; }
  return 0
}

cmd_prep() {
  set -e
  mkdir -p "$BASE" "$EXPORT" "$LOCAL" "$MNT" "$EMNT_P2" "$EMNT_P3"
  sudo modprobe erofs
  command -v mkfs.erofs >/dev/null || { echo "mkfs.erofs missing — apt-get install erofs-utils" >&2; exit 1; }
  [ -f "$BASE/stage/image.tar.gz" ] || { echo "stage/image.tar.gz missing" >&2; exit 1; }
  mv -f "$BASE/stage/image.tar.gz" "$EXPORT/image.tar.gz"
  log "building EROFS blob (5.4-readable profile: plain lz4, no dedupe/fragments/chunks)"
  rm -rf "$LOCAL/buildroot"; mkdir -p "$LOCAL/buildroot"
  gunzip -c "$EXPORT/image.tar.gz" | sudo tar -x -C "$LOCAL/buildroot"
  sudo mkfs.erofs -zlz4 "$EXPORT/blob.erofs" "$LOCAL/buildroot" >&2
  sudo chmod 644 "$EXPORT/blob.erofs"
  sudo rm -rf "$LOCAL/buildroot"
  sha256sum "$EXPORT/blob.erofs" | awk '{print $1}' > "$BASE/blob.sha256"
  # F30: the server refuses a blind export — stamp the volume-identity marker
  mkdir -p "$EXPORT/.flint-nfs"; printf '%s' "$VOLID" > "$EXPORT/.flint-nfs/volume-id"
  dump.erofs "$EXPORT/blob.erofs" >&2 || true
  log "prep done: $(ls -la "$EXPORT" | tail -2 | head -2 | awk '{print $9, $5}')"
}

cmd_server_start() {
  [ -f "$PIDFILE" ] && kill -0 "$(cat "$PIDFILE")" 2>/dev/null && { log "server already up"; return 0; }
  RUST_LOG=info nohup "$BASE/flint-nfs-server" \
    --export-path "$EXPORT" --volume-id "$VOLID" --port "$PORT" \
    > "$SRVLOG" 2>&1 &
  echo $! > "$PIDFILE"
  for i in $(seq 1 50); do  # poll, never check-once
    ss -tln | grep -q ":$PORT " && { log "server up on :$PORT pid $(cat "$PIDFILE")"; return 0; }
    sleep 0.2
  done
  bad "server never listened on :$PORT"; tail -5 "$SRVLOG" >&2; return 1
}

cmd_server_stop() {
  [ -f "$PIDFILE" ] || return 0
  kill "$(cat "$PIDFILE")" 2>/dev/null; rm -f "$PIDFILE"
}

rep_p1() { # baseline pull+start — transfer over flint mount + gunzip+untar + exec
  local t0 t1 out r_ops0 r_bytes0 r_ops1 r_bytes1 cpu0 cpu1
  remount_nfs || { bad "p1 remount"; return 1; }
  sudo rm -rf "$P1ROOT"; mkdir -p "$P1ROOT"
  read -r r_ops0 r_bytes0 <<<"$(mount_read_stats)"; cpu0=$(server_cpu_ms)
  t0=$(now_ns)
  cp "$MNT/image.tar.gz" "$LOCAL/image.tar.gz" && \
    gunzip -c "$LOCAL/image.tar.gz" | sudo tar -x -C "$P1ROOT" || { bad "p1 unpack"; return 1; }
  out=$(first_exec "$P1ROOT") || { bad "p1 exec"; return 1; }
  t1=$(now_ns)
  read -r r_ops1 r_bytes1 <<<"$(mount_read_stats)"; cpu1=$(server_cpu_ms)
  echo "{\"arm\":\"P1\",\"ready_ms\":$(( (t1-t0)/1000000 )),\"nfs_read_ops\":$((r_ops1-r_ops0)),\"nfs_read_bytes\":$((r_bytes1-r_bytes0)),\"server_cpu_ms\":$((cpu1-cpu0)),\"exec_out\":\"$out\"}"
  rm -f "$LOCAL/image.tar.gz"; sudo rm -rf "$P1ROOT"
}

rep_p2() { # format win — local blob, loop+erofs mount + exec (blob pages evicted)
  local t0 t1 out dev
  [ -f "$LOCAL/blob.erofs" ] || cp "$EXPORT/blob.erofs" "$LOCAL/blob.erofs"
  evict_file "$LOCAL/blob.erofs"
  t0=$(now_ns)
  dev=$(loop_mount "$LOCAL/blob.erofs" "$EMNT_P2") || { bad "p2 mount"; return 1; }
  out=$(first_exec "$EMNT_P2") || { bad "p2 exec"; loop_umount "$EMNT_P2" "$dev"; return 1; }
  t1=$(now_ns)
  echo "{\"arm\":\"P2\",\"ready_ms\":$(( (t1-t0)/1000000 )),\"exec_out\":\"$out\"}"
  loop_umount "$EMNT_P2" "$dev"
}

rep_p3() { # the remote-fault tax — blob on the flint mount, loop+erofs + exec
  local t0 t1 out dev r_ops0 r_bytes0 r_ops1 r_bytes1 cpu0 cpu1 warm_t0 warm_t1 warm_ops0 warm_ops1 warm_out
  remount_nfs || { bad "p3 remount"; return 1; }
  read -r r_ops0 r_bytes0 <<<"$(mount_read_stats)"; cpu0=$(server_cpu_ms)
  t0=$(now_ns)
  dev=$(loop_mount "$MNT/blob.erofs" "$EMNT_P3") || { bad "p3 mount"; return 1; }
  out=$(first_exec "$EMNT_P3") || { bad "p3 exec"; loop_umount "$EMNT_P3" "$dev"; return 1; }
  t1=$(now_ns)
  read -r r_ops1 r_bytes1 <<<"$(mount_read_stats)"; cpu1=$(server_cpu_ms)
  # G2 falsifiability, warm half: same mount, no eviction — the oracle must see
  # the lazy path collapse (near-zero new READ ops, collapsed exec time).
  read -r warm_ops0 _ <<<"$(mount_read_stats)"
  warm_t0=$(now_ns); warm_out=$(first_exec "$EMNT_P3"); warm_t1=$(now_ns)
  read -r warm_ops1 _ <<<"$(mount_read_stats)"
  echo "{\"arm\":\"P3\",\"ready_ms\":$(( (t1-t0)/1000000 )),\"nfs_read_ops\":$((r_ops1-r_ops0)),\"nfs_read_bytes\":$((r_bytes1-r_bytes0)),\"server_cpu_ms\":$((cpu1-cpu0)),\"exec_out\":\"$out\",\"warm_ready_ms\":$(( (warm_t1-warm_t0)/1000000 )),\"warm_read_ops\":$((warm_ops1-warm_ops0)),\"warm_exec_out\":\"$warm_out\"}"
  loop_umount "$EMNT_P3" "$dev"
}

cmd_fio() { # per-fault latency floor: qd1 randread O_DIRECT, 4k/64k/128k, both backings
  local bs backing name file
  echo '{"fio":['
  local first=1
  for backing in local nfs; do
    if [ "$backing" = local ]; then file="$LOCAL/blob.erofs"; else remount_nfs; file="$MNT/blob.erofs"; fi
    for bs in 4k 64k 128k; do
      [ $first -eq 1 ] && first=0 || echo ','
      sudo fio --name="pf-$backing-$bs" --filename="$file" --rw=randread --bs="$bs" \
        --iodepth=1 --direct=1 --runtime=10 --time_based --readonly \
        --output-format=json 2>/dev/null | \
      python3 -c "
import json,sys
j=json.load(sys.stdin); r=j['jobs'][0]['read']
pct=r.get('clat_ns',{}).get('percentile',{})
print(json.dumps({'backing':'$backing','bs':'$bs',
  'iops':round(r['iops'],1),
  'p50_us':round(pct.get('50.000000',0)/1000,1),
  'p99_us':round(pct.get('99.000000',0)/1000,1)}))"
    done
  done
  echo ']}'
}

cmd_sweep() { # G3: full-readability — EROFS profile drift shows up as EIO NOW
  local dev errs
  remount_nfs || { bad "sweep remount"; return 1; }
  dev=$(loop_mount "$MNT/blob.erofs" "$EMNT_P3") || { bad "sweep mount"; return 1; }
  errs=$( (sudo find "$EMNT_P3" -type f -exec cat {} + > /dev/null) 2>&1 | grep -c "Input/output error" || true)
  loop_umount "$EMNT_P3" "$dev"
  echo "{\"g3_eio_count\":${errs:-0}}"
}

cmd_g4() { # digest identity: blob via NFS == blob local == recorded sha
  local sha_local sha_nfs sha_ref
  sha_ref=$(cat "$BASE/blob.sha256")
  [ -f "$LOCAL/blob.erofs" ] || cp "$EXPORT/blob.erofs" "$LOCAL/blob.erofs"
  sha_local=$(sha256sum "$LOCAL/blob.erofs" | awk '{print $1}')
  remount_nfs || { bad "g4 remount"; return 1; }
  sha_nfs=$(sha256sum "$MNT/blob.erofs" | awk '{print $1}')
  echo "{\"g4_ref\":\"$sha_ref\",\"g4_local\":\"$sha_local\",\"g4_nfs\":\"$sha_nfs\",\"g4_ok\":$([ "$sha_ref" = "$sha_local" ] && [ "$sha_ref" = "$sha_nfs" ] && echo true || echo false)}"
}

cmd_run() {
  disk_guard || exit 1
  cmd_server_start || exit 1
  echo '{"reps":['
  local i first=1 load
  for i in $(seq 1 "$REPS"); do
    load=$(awk '{print $1}' /proc/loadavg)
    for arm in p1 p2 p3; do
      [ $first -eq 1 ] && first=0 || echo ','
      local line
      line=$("rep_$arm") || line="{\"arm\":\"${arm^^}\",\"failed\":true}"
      # stamp rep index + contention (shared VM: another session may be busy)
      echo "$line" | python3 -c "
import json,sys
d=json.load(sys.stdin); d['rep']=$i; d['loadavg']=$load
print(json.dumps(d))"
    done
    log "rep $i/$REPS done (loadavg $load)"
  done
  echo '],'
  echo '"guards":['; cmd_sweep; echo ','; cmd_g4; echo '],'
  # merge cmd_fio's {"fio":[...]} as a key of the root object (strip outer braces)
  cmd_fio | python3 -c 'import json,sys; print(json.dumps(json.load(sys.stdin))[1:-1])'
  echo ",\"env\":{\"kernel\":\"$(uname -r)\",\"port\":$PORT,\"reps\":$REPS,\"server\":\"v1.43.0 flint-nfs-server standalone\",\"fails\":\"$FAILS\"}}"
}

case "${1:-}" in
  prep) cmd_prep ;;
  server-start) cmd_server_start ;;
  server-stop) cmd_server_stop ;;
  run) cmd_run ;;
  sweep) cmd_sweep ;;
  fio) cmd_fio ;;
  clean) cmd_server_stop; sudo umount "$EMNT_P2" "$EMNT_P3" "$MNT" 2>/dev/null; sudo rm -rf "$LOCAL"; log clean ;;
  *) echo "usage: $0 prep|server-start|server-stop|run|sweep|fio|clean" >&2; exit 2 ;;
esac
