#!/bin/bash
# Gate d's small worlds on tlc-rs (2026-09-28, the user's call): replaces
# run-leanp1-small.sh (TLC) from the first world not yet in RESULTS.txt.
# LeanP1LiveHolds stays on TLC (liveness: tlc-rs keeps no checkpoint for it)
# and waits for SMALLDONE, which this runner writes. Each line is tagged with
# the checker. 6 workers, as TLC had; 6 h a world, as TLC had.
set -u
cd ${GATE_DIR:-~/lean-leanp1-2026-09-25d} || exit 1   # GATE_DIR, RECORD_TLIM: only to test the runner
BIN=$1; REV=$2
[ "$(md5sum LeanP1.tla | cut -c1-32)" = 3f6af642f7e39c19cafa413540e50cbe ] || { echo "MD5 CHANGED LeanP1.tla" >> RESULTS.txt; exit 4; }
ST=${GATE_ST:-/mnt/nvme2/tlcrs-leanp1d}; mkdir -p $ST out
LIMIT_KB=$((6 * 1024 * 1024))
while IFS=$'\t' read -r wld exp; do
  rm -rf $ST/$wld
  t0=$(date +%s)
  nice -n 10 $BIN -workers 6 -metadir $ST/$wld -config $wld.cfg $( [ -f $wld.tla ] && echo $wld.tla || echo LeanP1.tla ) > out/$wld-tlcrs.out 2>&1 &
  pid=$!; why=""
  # A RECORD world has no expected result, so it is capped at 1 h (the
  # user's call, to shorten the gate); the others keep TLC's 6 h.
  tlim=$( [ "$exp" = RECORD ] && echo ${RECORD_TLIM:-3600} || echo 21600 )
  while kill -0 $pid 2>/dev/null; do
    sleep 10
    rss=$(awk '/^VmRSS/{print $2}' /proc/$pid/status 2>/dev/null || echo 0)
    [ "${rss:-0}" -gt $LIMIT_KB ] && { why=GUARD-RSS; kill $pid; break; }
    [ $(( $(date +%s) - t0 )) -gt $tlim ] && { why=$( [ "$exp" = RECORD ] && echo RECORDED-CAP-1h || echo UNDECIDED-TIMEOUT ); kill $pid; break; }
  done
  wait $pid; rc=$?
  got=$(grep -oE "Invariant [A-Za-z_]+ is violated|Action property [A-Za-z_]+ is violated|Temporal property [A-Za-z_]+[^.]* is violated|No error has been found|^Error: [^.]*" out/$wld-tlcrs.out | head -1)
  v=MISMATCH
  if [ -n "$why" ]; then v=$why
  elif [ "$exp" = RECORD ]; then v=RECORDED
  elif [ "$exp" = HOLDS ] && [ "$got" = "No error has been found" ]; then v=OK-HOLDS
  elif [ "$exp" != HOLDS ] && echo "$got" | grep -q "$exp"; then v=OK-FIRES; fi
  printf "%-30s %-18s exp=%-22s rc=%-3s | %s | %s | %ss [tlc-rs %s]\n" "$wld" "$v" "$exp" "$rc" "$got" \
    "$(grep -oE '[0-9]+ states generated, [0-9]+ distinct states found. Depth [0-9]+' out/$wld-tlcrs.out | tail -1 | grep . || grep '^progress:' out/$wld-tlcrs.out | tail -1)" \
    $(( $(date +%s) - t0 )) "$REV" >> RESULTS.txt
  rm -rf $ST/$wld
# The worlds with an expected result first; the RECORD ones (no expected
# result, so they run to exhaustion or the time limit) last.
done < <(for rec in 0 1; do
  awk -F'\t' -v rec=$rec 'NR==FNR{d[$1]=1; next} !($1 in d) && $1 != "LeanP1LiveHolds" && (($2 == "RECORD") == rec)' \
    <(awk '{print $1}' RESULTS.txt) WORLDS-LeanP1.tsv
done)
echo SMALLDONE >> RESULTS.txt
