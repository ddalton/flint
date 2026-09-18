#!/usr/bin/env bash
# Paired arm: run the replace scenario against a NAMED binary.
#   usage: drill4.sh <path-to-flint-pnfs-mds> <label>
set -u
BIN="${1:?need a binary}"; LABEL="${2:-arm}"
PORT=20490; ROOT=/mnt/nvme/drill; EXPORT=$ROOT/export; STATE=$ROOT/state
MNT=/mnt/flintdrill; OUT=$ROOT/out-$LABEL; mkdir -p "$OUT"

echo "############ ARM: $LABEL ############"
printf 'BINARY UNDER TEST: %s  (%s bytes, mtime %s)\n' \
  "$BIN" "$(stat -c %s "$BIN")" "$(stat -c %y "$BIN" | cut -d. -f1)"

sudo umount -f "$MNT" 2>/dev/null
# ONE process name for every arm: `pgrep -x` is an EXACT match, so a
# control binary called flint-pnfs-mds.UNFIXED was invisible to it. The
# arm then declared "SERVER DID NOT START", bailed before its cleanup,
# and left that server holding the port — so the NEXT arm's client
# talked to the PREVIOUS arm's binary and reported its behaviour under
# the wrong label.
RUNNER=$ROOT/mds-under-test
for n in flint-pnfs-mds mds-under-test; do
  pgrep -x "$n" >/dev/null && { sudo pkill -x "$n"; sleep 2; }
done
cp -f "$BIN" "$RUNNER"
sudo rm -rf "$EXPORT" "$STATE"; mkdir -p "$EXPORT" "$STATE"; sudo mkdir -p "$MNT"
sudo "$RUNNER" --config "$ROOT/config.yaml" > "$OUT/server.log" 2>&1 &
SRVPID=$!
for i in $(seq 1 40); do ss -lnt 2>/dev/null | grep -q ":$PORT " && break; sleep 0.5; done
LIVE=$(pgrep -x mds-under-test | head -1)
[ -z "$LIVE" ] && { echo "SERVER DID NOT START"; tail -5 "$OUT/server.log"; exit 1; }
# Whoever is on the port must be the process THIS arm started.
OWNER=$(sudo ss -lntp 2>/dev/null | awk -v p=":$PORT" '"'"'$4 ~ p {print $0}'"'"' | grep -o "pid=[0-9]*" | head -1 | cut -d= -f2)
if [ -n "$OWNER" ] && [ "$OWNER" != "$LIVE" ]; then
  echo "PORT $PORT IS HELD BY pid=$OWNER, NOT this arm's server (pid=$LIVE) — refusing to measure"; exit 1
fi
echo "  server pid=$LIVE owns port $PORT"
sudo mount -t nfs4 -o minorversion=1,proto=tcp,port=$PORT 127.0.0.1:/ "$MNT" || { echo MOUNTFAIL; exit 1; }
case "$(stat -f -c %T "$MNT")" in nfs*) ;; *) echo "NOT AN NFS MOUNT"; exit 1;; esac
sudo chmod 777 "$MNT"

echo -n > "$MNT/data.db"
python3 -c "
import os
f=open('$MNT/data.db','r+b'); f.write(b'A'*4096); f.flush(); os.fsync(f.fileno()); f.close()"
INO_A=$(stat -c %i "$EXPORT/data.db")
ln "$MNT/data.db" "$MNT/oldlink"
exec 9<> "$MNT/data.db"                       # hold it open across the replace

echo -n > "$MNT/new.tmp"; mv "$MNT/new.tmp" "$MNT/data.db"
INO_B=$(stat -c %i "$EXPORT/data.db")
INO_CLI=$(stat -c %i "$MNT/data.db")
[ "$INO_A" = "$INO_B" ] && { echo "INCONCLUSIVE: inode reused"; exit 1; }

python3 -c "
import os
f=open('$MNT/data.db','r+b'); f.write(b'B'*4096); f.flush(); os.fsync(f.fileno()); f.close()
print('  client: write+fsync returned SUCCESS')"
exec 9>&-; sleep 2

DB_SIZE=$(stat -c %s "$EXPORT/data.db"); DB_FIRST=$(head -c 4 "$EXPORT/data.db" | cat -v)
OLD_FIRST=$(head -c 4 "$EXPORT/oldlink" | cat -v)
printf '  ino_A(replaced)=%s ino_B(current)=%s client-view=%s\n' "$INO_A" "$INO_B" "$INO_CLI"
printf '  data.db: size=%s first=[%s]\n' "$DB_SIZE" "$DB_FIRST"
printf '  oldlink(replaced inode): first=[%s]\n' "$OLD_FIRST"
if [ "$DB_FIRST" = "BBBB" ] && [ "$OLD_FIRST" = "AAAA" ]; then
  echo "  RESULT[$LABEL]: CORRECT — the write landed in the file the path names"
elif [ "$OLD_FIRST" = "BBBB" ]; then
  echo "  RESULT[$LABEL]: DATA LOSS — the write landed in the REPLACED inode"
else
  echo "  RESULT[$LABEL]: UNEXPECTED — inspect $OUT"
fi
sudo umount -f "$MNT" 2>/dev/null; sudo pkill -x mds-under-test 2>/dev/null
