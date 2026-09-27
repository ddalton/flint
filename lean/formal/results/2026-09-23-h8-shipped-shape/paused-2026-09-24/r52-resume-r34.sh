#!/bin/bash
# r52 — resume r34 (3-PATH LeanImmutableRenameHolds on LeanSubtree md5 d2dd061a…)
# from its checkpoint of 2026-09-24 05:20:35 (paused for the night; box shut down).
# SOUNDNESS FINGERPRINT: "Recovery completed" must report ~1,465.6M states
# examined and ~96.15M on queue (Progress(30) at the checkpoint: 1,465,616,138
# distinct, queue 96,154,940). Anything else: STOP, do not trust the run.
set -u
cd ~/lean-gate-2026-09-19io/lean/formal || exit 1
[ "$(md5sum LeanSubtree.tla | cut -c1-32)" = d2dd061afb75ee6d937bf22d20d0268b ] || { echo "MODULE MD5 CHANGED — REFUSING" > ~/lean-core-r3/r52.verdicts; exit 4; }
[ "$(md5sum LeanImmutableRenameHolds.cfg | cut -c1-32)" = b50442b3735487efe965a4857fad3b72 ] || { echo "CFG MD5 CHANGED — REFUSING" > ~/lean-core-r3/r52.verdicts; exit 4; }
OUT=~/lean-core-r3/r52-RenameHolds-3path-recovered.out
java -XX:+UseParallelGC -Xmx12g -cp ~/lean-gate-2026-09-18/.tla2tools.jar tlc2.TLC -workers 8 \
  -metadir /mnt/nvme2/tlc-r27 -recover /mnt/nvme2/tlc-r27/26-09-22-18-50-35 \
  -config LeanImmutableRenameHolds.cfg LeanSubtree.tla > "$OUT" 2>&1
rc=$?
{
  echo "r52 RenameHolds 3-PATH (r34 resumed after the 2026-09-24 pause) rc=$rc"
  grep -oE "Invariant [A-Za-z_]+ is violated|Property [A-Za-z_]+ is violated|No error has been found" "$OUT" | head -1
  grep -oE "^[0-9,]+ states generated, [0-9,]+ distinct" "$OUT" | tail -1
  grep -oE "The depth of the complete state graph search is [0-9]+" "$OUT" | tail -1
  grep -E "^Recovery completed" "$OUT" | head -1
} > ~/lean-core-r3/r52.verdicts
echo R52DONE >> ~/lean-core-r3/r52.verdicts
