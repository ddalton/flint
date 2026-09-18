#!/usr/bin/env bash
# Per-RPC cost of the inode guard. Small ops only: a bulk stream is
# dominated by the network and would hide a ~1us syscall completely.
#   usage: perf.sh <binary> <label> [reps]
set -u
BIN="${1:?}"; LABEL="${2:?}"; REPS="${3:-3}"
PORT=20490; ROOT=/mnt/nvme/drill; EXPORT=$ROOT/export; STATE=$ROOT/state
MNT=/mnt/flintdrill; RUNNER=$ROOT/mds-under-test; OUT=$ROOT/perf-$LABEL; mkdir -p "$OUT"

for n in flint-pnfs-mds mds-under-test; do pgrep -x "$n" >/dev/null && { sudo pkill -x "$n"; sleep 2; }; done
sudo umount -f "$MNT" 2>/dev/null
sudo rm -rf "$EXPORT" "$STATE"; mkdir -p "$EXPORT" "$STATE"; sudo mkdir -p "$MNT"
cp -f "$BIN" "$RUNNER"
sudo "$RUNNER" --config "$ROOT/config.yaml" > "$OUT/server.log" 2>&1 &
for i in $(seq 1 40); do ss -lnt 2>/dev/null | grep -q ":$PORT " && break; sleep 0.5; done
LIVE=$(pgrep -x mds-under-test | head -1)
[ -z "$LIVE" ] && { echo "$LABEL: SERVER DID NOT START"; exit 1; }
sudo mount -t nfs4 -o minorversion=1,proto=tcp,port=$PORT 127.0.0.1:/ "$MNT" || { echo "$LABEL: MOUNTFAIL"; exit 1; }
case "$(stat -f -c %T "$MNT")" in nfs*) ;; *) echo "$LABEL: NOT NFS"; exit 1;; esac
sudo chmod 777 "$MNT"

printf '%s\t%s\t' "$LABEL" "$(stat -c %s "$BIN")"
for r in $(seq 1 "$REPS"); do
  rm -rf "$MNT/w"; mkdir -p "$MNT/w"
  # A: 2000 small writes+reads on ONE hot file -> the cache-HIT path (+1 stat each)
  HOT=$( { /usr/bin/time -f %e python3 -c "
import os
p='$MNT/w/hot.bin'
f=open(p,'w+b')
for i in range(2000):
    f.seek((i%64)*4096); f.write(b'x'*4096); f.flush()
    f.seek((i%64)*4096); f.read(4096)
f.close()" ; } 2>&1 | tail -1 )
  # B: 400 create/write/close -> the cache-MISS path (where the double stat was)
  MISS=$( { /usr/bin/time -f %e python3 -c "
import os
for i in range(400):
    p='$MNT/w/f%d'%i
    f=open(p,'w+b'); f.write(b'y'*4096); f.close()" ; } 2>&1 | tail -1 )
  printf 'hot=%s miss=%s  ' "$HOT" "$MISS"
done
echo
sudo umount -f "$MNT" 2>/dev/null; sudo pkill -x mds-under-test 2>/dev/null
