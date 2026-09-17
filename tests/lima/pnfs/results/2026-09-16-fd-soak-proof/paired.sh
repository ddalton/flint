#!/usr/bin/env bash
# PROOF 2 (strengthened) — IS IT THE CLIENT OR THE SERVER?
#
# The same client kernel, the same sqlite workload, two NFSv4.1 servers.
# knfsd reports its own held descriptors in /proc/fs/nfsd/filecache
# ("total inodes"); flint's are its process fds on the db file. If the
# client is simply holding opens across transactions, BOTH grow. If only
# flint grows, the client is not the cause.
set -u
N=${N:-2000}; STEP=${STEP:-500}
P=$(pgrep -f flint-pnfs-mds | head -1); [ -n "$P" ] || { echo "NO HUB"; exit 2; }
fl(){ sudo ls -l /proc/$P/fd 2>/dev/null | grep -c 'flintexport/paired.db'; }
kn(){ awk '/^total inodes/{print $3}' /proc/fs/nfsd/filecache; }

for M in /mnt/flint /mnt/knfsd; do
  rm -f $M/paired.db*
  sqlite3 $M/paired.db "CREATE TABLE t(v int);" || { echo "SETUP FAILED on $M"; exit 2; }
done
sync; sleep 2
echo "start: flint_fds_on_db=$(fl)  knfsd_total_inodes=$(kn)"
echo "txn,flint_fds_on_db,knfsd_total_inodes"
ff=0; kf=0
for i in $(seq 1 $N); do
  sqlite3 /mnt/flint/paired.db ".timeout 10000" "INSERT INTO t VALUES($i);" >/dev/null 2>&1 || ff=$((ff+1))
  sqlite3 /mnt/knfsd/paired.db ".timeout 10000" "INSERT INTO t VALUES($i);" >/dev/null 2>&1 || kf=$((kf+1))
  [ $((i % STEP)) -eq 0 ] && echo "$i,$(fl),$(kn)"
done
sleep 5
FR=$(sqlite3 /mnt/flint/paired.db 'select count(*) from t;' 2>&1)
KR=$(sqlite3 /mnt/knfsd/paired.db 'select count(*) from t;' 2>&1)
echo "end: flint_fds_on_db=$(fl)  knfsd_total_inodes=$(kn)"
echo "flint rows=$FR failures=$ff / knfsd rows=$KR failures=$kf  (both must be $N)"
echo "knfsd filecache:"; sed 's/^/  /' /proc/fs/nfsd/filecache
if [ "$FR" != "$N" ] || [ "$KR" != "$N" ]; then echo "VERDICT=INCONCLUSIVE (a workload did not complete)"; exit 1; fi
echo "VERDICT=MEASURED"
