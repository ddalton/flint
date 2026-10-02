#!/bin/bash
# Re-run LeanP1's unscoped worlds on the model with scope and rescope
# (2026-10-02), on the Mac with tlc-rs. Usage: run-unscoped-rerun.sh <tlc-rs> <rev> <statesdir>
# Skipped (box jobs, >400M states): LeanP1Holds (the deep run), LeanP1LiveHolds,
# LeanP1DeleteOverrideOff, LeanP1Holds1p3b; RECORD worlds (no expected result).
set -u
cd "$(dirname "$0")/../.." || exit 1
BIN=$1; REV=$2; ST=$3; OUT=results/2026-10-02-leanp1-scope
R=$OUT/RESULTS-unscoped-rerun.txt
echo "LeanP1.tla $(md5 -q LeanP1.tla), tlc-rs $REV, 4 workers; started $(date -u +%FT%TZ)" >> $R
mkdir -p "$ST" $OUT/out
SKIP=" LeanP1Holds LeanP1LiveHolds LeanP1DeleteOverrideOff LeanP1Holds1p3b "
run() { # <world> <expect> <cap s> <module>
  local wld=$1 exp=$2 cap=$3 mod=$4 t0 rc got v
  rm -rf "${ST:?}/$wld"; t0=$(date +%s)
  timeout $cap $BIN -workers 4 -metadir "$ST/$wld" -config $wld.cfg $mod > $OUT/out/$wld.out 2>&1; rc=$?
  got=$(grep -oE "Invariant [A-Za-z_]+ is violated|Action property [A-Za-z_]+ is violated|Temporal propert[a-z]* [^.]*violated|No error has been found|^Error: [^.]*" $OUT/out/$wld.out | head -1)
  v=MISMATCH
  if [ $rc = 124 ]; then v=UNDECIDED-TIMEOUT
  elif [ "$exp" = HOLDS ] && [ "$got" = "No error has been found" ]; then v=OK-HOLDS
  elif [ "$exp" != HOLDS ] && echo "$got" | grep -q "$exp"; then v=OK-FIRES; fi
  printf "%-32s %-17s exp=%-24s rc=%-3s | %s | %s | %ss\n" "$wld" "$v" "$exp" "$rc" "$got" \
    "$(grep -oE '[0-9]+ states generated, [0-9]+ distinct states found. Depth [0-9]+' $OUT/out/$wld.out | tail -1)" $(( $(date +%s) - t0 )) >> $R
  rm -rf "${ST:?}/$wld"
}
while IFS=$'\t' read -r wld exp; do
  case "$SKIP" in *" $wld "*) continue;; esac
  [ "$exp" = RECORD ] && continue
  case $wld in LeanP1Scope*|LeanP1WidenOverwrites|LeanP1NarrowUnlinkFirst|LeanP1UnlinkBlind|LeanP1ProbeNarrowed|LeanP1ProbeWidened|LeanP1ProbeRescopeReplayed|LeanP1ProbeOutOfScopePublished) continue;; esac
  run $wld $exp 1800 LeanP1.tla
done < WORLDS-LeanP1.tsv
run MCLeanP1Like HOLDS 7200 MCLeanP1Like.tla
echo "DONE $(date -u +%FT%TZ)" >> $R
