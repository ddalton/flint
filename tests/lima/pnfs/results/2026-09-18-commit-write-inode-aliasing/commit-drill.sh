#!/usr/bin/env bash
# Live drill: does COMMIT fsync the REPLACED inode, against a real kernel
# NFSv4.1 client? Traced at the kernel with bpftrace, so the evidence is
# which inode the server actually fsync'd -- not what the cache returned.
set -u
PORT=20490
ROOT=/mnt/nvme/drill
EXPORT=$ROOT/export
STATE=$ROOT/state
MNT=/mnt/flintdrill
BIN=$HOME/flint/spdk-csi-driver/target/debug/flint-pnfs-mds
OUT=$ROOT/out; mkdir -p "$OUT"

say() { printf '\n== %s ==\n' "$*"; }

# ---- clean slate (exact process name: never a -f pattern that could match this script)
sudo umount -f "$MNT" 2>/dev/null
pgrep -x flint-pnfs-mds >/dev/null && { sudo pkill -x flint-pnfs-mds; sleep 2; }
sudo rm -rf "$EXPORT" "$STATE"; mkdir -p "$EXPORT" "$STATE"; sudo mkdir -p "$MNT"

cat > "$ROOT/config.yaml" <<CFG
apiVersion: chert.us/v1alpha1
kind: PnfsConfig
mode: standalone
mds:
  bind: { address: "0.0.0.0", port: $PORT }
  layout: { type: file, stripeSize: 8388608, policy: stripe }
  dataServers: []
  state: { backend: sqlite, config: { path: $STATE/state.db } }
  ha: { enabled: false, replicas: 1, leaderElection: false }
exports:
  - path: $EXPORT
    fsid: 1
    options: [rw, sync, no_subtree_check]
    access: [{ network: 0.0.0.0/0, permissions: rw }]
logging: { level: info, format: text, components: { mds: info } }
monitoring:
  prometheus: { enabled: false, port: 0, path: /metrics }
  health: { enabled: true, port: 20491, path: /health }
  metrics: []
CFG

say "start server"
sudo "$BIN" --config "$ROOT/config.yaml" > "$OUT/server.log" 2>&1 &
for i in $(seq 1 40); do ss -lnt 2>/dev/null | grep -q ":$PORT " && break; sleep 0.5; done
SRVPID=$(pgrep -x flint-pnfs-mds | head -1)
[ -z "$SRVPID" ] && { echo "SERVER DID NOT START"; tail -20 "$OUT/server.log"; exit 1; }
echo "server pid=$SRVPID"

say "mount"
sudo mount -t nfs4 -o minorversion=1,proto=tcp,port=$PORT 127.0.0.1:/ "$MNT" || { echo "MOUNT FAILED"; exit 1; }
FSTYPE=$(stat -f -c %T "$MNT")
echo "fstype at the consumer: $FSTYPE"
case "$FSTYPE" in nfs*) ;; *) echo "NOT AN NFS MOUNT -- every result below would be about local disk"; exit 1;; esac
sudo chmod 777 "$MNT"

say "arm bpftrace on the server's fsync path"
sudo bpftrace -e "kprobe:vfs_fsync_range /pid == $SRVPID/ { printf(\"FSYNC ino=%lu\n\", ((struct file *)arg0)->f_inode->i_ino); }" > "$OUT/fsync.log" 2>"$OUT/bpftrace.err" &
BPFPID=$!
sleep 4   # let the probe attach

# ============ CONTROL LEG: no replace. COMMIT must fsync the file itself.
say "CONTROL leg (no replace)"
echo -n > "$MNT/ctl.db"
CTL_INO=$(stat -c %i "$EXPORT/ctl.db")
python3 - "$MNT/ctl.db" <<'PY'
import sys,os
f=open(sys.argv[1],'r+b'); f.write(b'C'*4096); f.flush(); os.fsync(f.fileno()); f.close()
PY
sleep 2
echo "control: file ino=$CTL_INO"

# ============ BUG LEG: replace the file at the same path, then COMMIT.
say "BUG leg (replace at the same path)"
echo -n > "$MNT/data.db"
python3 - "$MNT/data.db" <<'PY'
import sys,os
f=open(sys.argv[1],'r+b'); f.write(b'A'*4096); f.flush(); os.fsync(f.fileno()); f.close()
PY
INO_A=$(stat -c %i "$EXPORT/data.db")
echo "wrote v1: ino_A=$INO_A"

# hold it open on the client so no CLOSE is sent and the server's cache
# entry for this path survives the replace (a DB or a tailer does this)
exec 9<> "$MNT/data.db"

# the write-tmp-and-rename-over pattern
echo -n > "$MNT/new.tmp"
mv "$MNT/new.tmp" "$MNT/data.db"
INO_B=$(stat -c %i "$EXPORT/data.db")
echo "replaced:  ino_B=$INO_B  (ino_A now unlinked)"
[ "$INO_A" = "$INO_B" ] && { echo "INCONCLUSIVE: the replace reused the inode"; }

sleep 1
MARK=$(wc -l < "$OUT/fsync.log")
python3 - "$MNT/data.db" <<'PY'
import sys,os
f=open(sys.argv[1],'r+b'); f.write(b'B'*4096); f.flush(); os.fsync(f.fileno()); f.close()
PY
sleep 3
exec 9>&-

say "RESULTS"
echo "ino_CTL=$CTL_INO  ino_A(replaced)=$INO_A  ino_B(current)=$INO_B"
echo "--- fsyncs traced AFTER the replace (the COMMIT under test) ---"
tail -n +$((MARK+1)) "$OUT/fsync.log" | sort | uniq -c
echo "--- all traced fsyncs ---"
sort "$OUT/fsync.log" | uniq -c

sudo kill $BPFPID 2>/dev/null
sudo umount -f "$MNT" 2>/dev/null
sudo pkill -x flint-pnfs-mds 2>/dev/null
echo "DRILL_DONE"
