#!/bin/bash
# r34 — resume r28 (the 3-PATH world) once r32 has finished, so the two
# never contend (together they halved the completable run's throughput).
# r28's last checkpoint: 14:49:34. SOUNDNESS FINGERPRINT: must resume at
# DEPTH 25, ~701.0M distinct, queue ~160.8M. Same 8 workers as r27/r28.
set -u
while pgrep -f "tlc-r30" >/dev/null; do sleep 60; done
grep -q R32DONE ~/lean-core-r3/r32.verdicts 2>/dev/null || { echo "r32 did not finish cleanly — NOT starting r28" > ~/lean-core-r3/r34.verdicts; exit 5; }
cd ~/lean-gate-2026-09-19io/lean/formal || exit 1
[ "$(md5sum LeanSubtree.tla | cut -c1-32)" = d2dd061afb75ee6d937bf22d20d0268b ] || { echo "MODULE MD5 CHANGED — REFUSING" > ~/lean-core-r3/r34.verdicts; exit 4; }
OUT=~/lean-core-r3/r34-RenameHolds-3path-recovered.out
java -XX:+UseParallelGC -Xmx12g -cp ~/lean-gate-2026-09-18/.tla2tools.jar tlc2.TLC -workers 8 \
  -metadir /mnt/nvme2/tlc-r27 -recover /mnt/nvme2/tlc-r27/26-09-22-18-50-35 \
  -config LeanImmutableRenameHolds.cfg LeanSubtree.tla > "$OUT" 2>&1
rc=$?
{
  echo "r34 RenameHolds 3-PATH conj3fix RECOVERED (again) rc=$rc"
  grep -oE "Invariant [A-Za-z_]+ is violated|Property [A-Za-z_]+ is violated|No error has been found" "$OUT" | head -1
  grep -oE "^[0-9,]+ states generated, [0-9,]+ distinct" "$OUT" | tail -1
  grep -oE "The depth of the complete state graph search is [0-9]+" "$OUT" | tail -1
  grep -E "^Recovery completed" "$OUT" | head -1
} > ~/lean-core-r3/r34.verdicts
echo R34DONE >> ~/lean-core-r3/r34.verdicts
