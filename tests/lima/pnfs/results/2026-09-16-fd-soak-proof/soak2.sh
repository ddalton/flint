#!/usr/bin/env bash
# PROOF 1 — SCALE, WITH A POSITIVE CONTROL.
#
# 60 transactions is not evidence that a bound holds. But a flat line is
# only evidence if the SAME measurement, run against a build with the fix
# removed, CLIMBS. Otherwise flat could mean "the workload never opened
# anything" and I would not be able to tell.
#
# ARM: $1 = the binary to run. Everything else is identical.
set -u
BIN=$1; TAG=$2; N=${N:-4000}; STEP=${STEP:-250}
P=$(pgrep -f "flint-pnfs-mds" | head -1); [ -n "$P" ] || { echo "$TAG: NO HUB — cannot measure"; exit 2; }

# The workload must actually do work. If the table is missing every INSERT
# fails, the server opens nothing, and a flat line would be a lie.
rm -f /mnt/flint/soak.db*
sqlite3 /mnt/flint/soak.db "CREATE TABLE t(v int);" || { echo "$TAG: SETUP FAILED"; exit 2; }

B=$(sudo ls /proc/$P/fd | wc -l)
echo "$TAG baseline_fds=$B"
echo "$TAG txns,fds"
fail=0
for i in $(seq 1 $N); do
  sqlite3 /mnt/flint/soak.db ".timeout 10000" "INSERT INTO t VALUES($i);" >/dev/null 2>&1 || fail=$((fail+1))
  if [ $((i % STEP)) -eq 0 ]; then
    echo "$TAG $i,$(sudo ls /proc/$P/fd | wc -l)"
  fi
done
sleep 5
ROWS=$(sqlite3 /mnt/flint/soak.db 'select count(*) from t;' 2>&1)
F=$(sudo ls /proc/$P/fd | wc -l)
echo "$TAG final_fds=$F baseline=$B delta=$((F-B))"
echo "$TAG rows=$ROWS expected=$N insert_failures=$fail"
# The workload must have RUN for the fd number to mean anything.
if [ "$ROWS" != "$N" ]; then echo "$TAG VERDICT=INCONCLUSIVE (workload did not complete: $ROWS/$N rows)"; exit 1; fi
echo "$TAG VERDICT=MEASURED"
