#!/usr/bin/env bash
# Count newfstatat(2) in the SERVER across an identical workload.
# Deterministic where timing is not: the question is "how many extra
# stats per RPC", not "can a 1us syscall be seen through RPC noise".
set -u
BIN="${1:?}"; LABEL="${2:?}"
PORT=20490; ROOT=/mnt/nvme/drill; EXPORT=$ROOT/export; STATE=$ROOT/state
MNT=/mnt/flintdrill; RUNNER=$ROOT/mds-under-test; OUT=$ROOT/cnt-$LABEL; mkdir -p "$OUT"

for n in flint-pnfs-mds mds-under-test; do pgrep -x "$n" >/dev/null && { sudo pkill -x "$n"; sleep 2; }; done
sudo umount -f "$MNT" 2>/dev/null
sudo rm -rf "$EXPORT" "$STATE"; mkdir -p "$EXPORT" "$STATE"; sudo mkdir -p "$MNT"
cp -f "$BIN" "$RUNNER"
sudo "$RUNNER" --config "$ROOT/config.yaml" > "$OUT/server.log" 2>&1 &
for i in $(seq 1 40); do ss -lnt 2>/dev/null | grep -q ":$PORT " && break; sleep 0.5; done
PID=$(pgrep -x mds-under-test | head -1)
[ -z "$PID" ] && { echo "$LABEL: SERVER DID NOT START"; exit 1; }
sudo mount -t nfs4 -o minorversion=1,proto=tcp,port=$PORT 127.0.0.1:/ "$MNT" || { echo "$LABEL: MOUNTFAIL"; exit 1; }
case "$(stat -f -c %T "$MNT")" in nfs*) ;; *) echo "$LABEL: NOT NFS"; exit 1;; esac
sudo chmod 777 "$MNT"
mkdir -p "$MNT/w"

# Count stat-family + WRITE RPCs served, over a FIXED workload.
sudo timeout 120 bpftrace -e "
tracepoint:syscalls:sys_enter_newfstatat /pid == $PID/ { @stat = count(); }
tracepoint:syscalls:sys_enter_pwrite64   /pid == $PID/ { @pwrite = count(); }
tracepoint:syscalls:sys_enter_openat     /pid == $PID/ { @openat = count(); }
" > "$OUT/counts.txt" 2>"$OUT/bpf.err" &
BPF=$!
sleep 4
python3 -c "
import os
for i in range(200):
    p='$MNT/w/f%d'%i
    f=open(p,'w+b'); f.write(b'y'*4096); f.flush(); os.fsync(f.fileno()); f.close()
"
sleep 2
sudo kill -TERM $BPF 2>/dev/null; wait $BPF 2>/dev/null
printf '%-12s ' "$LABEL"
grep -E "@(stat|pwrite|openat)" "$OUT/counts.txt" | tr -d ' ' | tr '\n' ' '
echo
sudo umount -f "$MNT" 2>/dev/null; sudo pkill -x mds-under-test 2>/dev/null
