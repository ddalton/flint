#!/usr/bin/env bash
# Drill v3: is it the SERVER picking a stale fd, or the CLIENT sending a
# stale filehandle? Compare the client's view of the inode with the server's.
set -u
PORT=20490; ROOT=/mnt/nvme/drill; EXPORT=$ROOT/export; STATE=$ROOT/state
MNT=/mnt/flintdrill; BIN=$HOME/flint/spdk-csi-driver/target/debug/flint-pnfs-mds
OUT=$ROOT/out3; mkdir -p "$OUT"
say(){ printf '\n== %s ==\n' "$*"; }

sudo umount -f "$MNT" 2>/dev/null
pgrep -x flint-pnfs-mds >/dev/null && { sudo pkill -x flint-pnfs-mds; sleep 2; }
sudo rm -rf "$EXPORT" "$STATE"; mkdir -p "$EXPORT" "$STATE"; sudo mkdir -p "$MNT"
sed 's/level: info/level: debug/; s/mds: info/mds: debug/' "$ROOT/config.yaml" > "$ROOT/config-dbg.yaml"
sudo "$BIN" --config "$ROOT/config-dbg.yaml" > "$OUT/server.log" 2>&1 &
for i in $(seq 1 40); do ss -lnt 2>/dev/null | grep -q ":$PORT " && break; sleep 0.5; done
sudo mount -t nfs4 -o minorversion=1,proto=tcp,port=$PORT 127.0.0.1:/ "$MNT" || exit 1
case "$(stat -f -c %T "$MNT")" in nfs*) ;; *) echo "NOT NFS"; exit 1;; esac
sudo chmod 777 "$MNT"

echo -n > "$MNT/data.db"
python3 -c "
import os
f=open('$MNT/data.db','r+b'); f.write(b'A'*4096); f.flush(); os.fsync(f.fileno()); f.close()"
INO_A=$(stat -c %i "$EXPORT/data.db")
ln "$MNT/data.db" "$MNT/oldlink"
exec 9<> "$MNT/data.db"

echo -n > "$MNT/new.tmp"; mv "$MNT/new.tmp" "$MNT/data.db"
INO_B_SRV=$(stat -c %i "$EXPORT/data.db")

say "WHOSE VIEW IS STALE?"
INO_B_CLI=$(stat -c %i "$MNT/data.db")
echo "  server sees data.db as ino=$INO_B_SRV"
echo "  CLIENT sees data.db as ino=$INO_B_CLI   (via the mount)"
if [ "$INO_B_CLI" = "$INO_A" ]; then
  echo "  -> the CLIENT is holding a stale filehandle; a write landing in ino_A is the"
  echo "     client's doing, NOT the server's fd cache."
elif [ "$INO_B_CLI" = "$INO_B_SRV" ]; then
  echo "  -> the client sees the NEW inode. If bytes still land in ino_A, that is the"
  echo "     SERVER choosing a stale cached descriptor."
fi

say "write B-bytes through the client and fsync"
MARK=$(wc -l < "$OUT/server.log")
python3 -c "
import os
f=open('$MNT/data.db','r+b'); f.write(b'B'*4096); f.flush(); os.fsync(f.fileno()); f.close()
print('  client: write+fsync returned SUCCESS')"
exec 9>&-; sleep 2

say "RESULT"
printf '  ino_A(replaced)=%s  ino_B(current)=%s  client-view=%s\n' "$INO_A" "$INO_B_SRV" "$INO_B_CLI"
printf '  data.db size=%s  oldlink(ino_A) size=%s first=%s\n' \
  "$(stat -c %s "$EXPORT/data.db")" "$(stat -c %s "$EXPORT/oldlink")" \
  "$(head -c 4 "$EXPORT/oldlink" | cat -v)"

say "server WRITE log lines after the replace"
tail -n +$((MARK+1)) "$OUT/server.log" | grep -iE "WRITE|COMMIT|FD CACHE|adopt" | head -20

sudo umount -f "$MNT" 2>/dev/null; sudo pkill -x flint-pnfs-mds 2>/dev/null
echo "DRILL3_DONE"
