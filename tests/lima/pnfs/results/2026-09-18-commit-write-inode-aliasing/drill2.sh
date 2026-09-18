#!/usr/bin/env bash
# Drill v2: WHERE did the bytes go? Keep the replaced inode reachable via
# a hardlink so it can be read back after the replace.
set -u
PORT=20490; ROOT=/mnt/nvme/drill; EXPORT=$ROOT/export; STATE=$ROOT/state
MNT=/mnt/flintdrill; BIN=$HOME/flint/spdk-csi-driver/target/debug/flint-pnfs-mds
OUT=$ROOT/out2; mkdir -p "$OUT"
say(){ printf '\n== %s ==\n' "$*"; }

sudo umount -f "$MNT" 2>/dev/null
pgrep -x flint-pnfs-mds >/dev/null && { sudo pkill -x flint-pnfs-mds; sleep 2; }
sudo rm -rf "$EXPORT" "$STATE"; mkdir -p "$EXPORT" "$STATE"; sudo mkdir -p "$MNT"
sudo "$BIN" --config "$ROOT/config.yaml" > "$OUT/server.log" 2>&1 &
for i in $(seq 1 40); do ss -lnt 2>/dev/null | grep -q ":$PORT " && break; sleep 0.5; done
SRVPID=$(pgrep -x flint-pnfs-mds | head -1); echo "server pid=$SRVPID"
sudo mount -t nfs4 -o minorversion=1,proto=tcp,port=$PORT 127.0.0.1:/ "$MNT" || exit 1
case "$(stat -f -c %T "$MNT")" in nfs*) ;; *) echo "NOT NFS"; exit 1;; esac
sudo chmod 777 "$MNT"

say "v1: write A-bytes to data.db"
echo -n > "$MNT/data.db"
python3 -c "
import sys,os
f=open('$MNT/data.db','r+b'); f.write(b'A'*4096); f.flush(); os.fsync(f.fileno()); f.close()"
INO_A=$(stat -c %i "$EXPORT/data.db"); echo "ino_A=$INO_A size=$(stat -c %s "$EXPORT/data.db")"

# keep ino_A reachable under a second name so we can read it back later
ln "$MNT/data.db" "$MNT/oldlink"
exec 9<> "$MNT/data.db"           # hold it open: no CLOSE, cache entry survives

say "replace data.db at the same path"
echo -n > "$MNT/new.tmp"; mv "$MNT/new.tmp" "$MNT/data.db"
INO_B=$(stat -c %i "$EXPORT/data.db"); echo "ino_B=$INO_B"
[ "$INO_A" = "$INO_B" ] && { echo "INCONCLUSIVE: inode reused"; exit 1; }

say "v2: write B-bytes to data.db (the NEW inode) and fsync"
python3 -c "
import sys,os
f=open('$MNT/data.db','r+b'); f.write(b'B'*4096); f.flush(); os.fsync(f.fileno()); f.close()
print('client: write+fsync returned SUCCESS')"
exec 9>&-
sleep 2

say "WHERE ARE THE BYTES? (read from the SERVER's filesystem)"
printf '  data.db (ino_B=%s) size=%s first-bytes=%s\n' "$INO_B" \
  "$(stat -c %s "$EXPORT/data.db")" "$(head -c 4 "$EXPORT/data.db" | tr -d '\0' | cat -v)"
printf '  oldlink (ino_A=%s) size=%s first-bytes=%s\n' "$(stat -c %i "$EXPORT/oldlink")" \
  "$(stat -c %s "$EXPORT/oldlink")" "$(head -c 4 "$EXPORT/oldlink" | cat -v)"
echo
echo "  VERDICT:"
if [ "$(stat -c %s "$EXPORT/data.db")" = "0" ] && [ "$(head -c 1 "$EXPORT/oldlink")" = "B" ]; then
  echo "  *** DATA LOSS: the client's write+fsync succeeded, but the bytes landed in the"
  echo "  *** REPLACED inode ($INO_A). The file at that path ($INO_B) is EMPTY."
elif [ "$(head -c 1 "$EXPORT/data.db")" = "B" ]; then
  echo "  bytes landed correctly in ino_B -- no loss on this path"
else
  echo "  unexpected state -- inspect by hand"
fi

sudo umount -f "$MNT" 2>/dev/null; sudo pkill -x flint-pnfs-mds 2>/dev/null
echo "DRILL2_DONE"
