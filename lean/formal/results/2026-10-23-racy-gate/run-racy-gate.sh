#!/bin/bash
# The racy model with the checkout's gate (latency plan §17): every world on
# tla-mc, then the proof's RECORD run and one CONTROL per rule detail
# (WritesWaitForGate = FALSE among them, which must fail).
D=/mnt/nvme/lean-racy-gate; cd $D || exit 1
TLAMC=/mnt/nvme/tla-mc-0.1.2/bin/tla-mc
for c in LeanScanRacy LeanScanRacyWide LeanScanRacyBefore LeanScanRacyNoRule LeanScanRacyMeasureLate \
         LeanScanRacyClearEarly LeanScanRacyNoGate LeanScanRacyProbe LeanScanRacyProbeGate; do
  nice -n 10 $TLAMC -workers 2 -config $c.cfg LeanScanRacy.tla > tlc-$c.out 2>&1
  printf "%-26s %s | %s\n" $c "$(grep -m1 -oE 'No error has been found|Invariant [A-Za-z_]+ is violated|Action property [A-Za-z_]+ is violated|Temporal properties were violated|[Ee]rror:.*' tlc-$c.out)" \
    "$(grep -m1 -oE '[0-9,]+ distinct states found' tlc-$c.out)" >> TLC.txt
done
T=/mnt/nvme/tlaps/unpack/tlapm/bin/tlapm
OPTS="--toolbox 0 0 --cleanfp --stretch 4 --threads 1"
{ echo "RECORD $(date -u +%Y-%m-%dT%H:%M:%SZ) tlapm $($T --version 2>&1 | head -1)"
  md5sum LeanScanRacy.tla LeanScanRacyProof.tla
  /usr/bin/time -v nice -n 10 $T $OPTS LeanScanRacyProof.tla 2>&1; echo "rc=$?"; } > record.out 2>&1
for m in RacyClean MeasureAtRead ClearOutsideWindow WritesWaitForGate; do
  C=$D/ctl-$m; rm -rf $C; mkdir -p $C; cp LeanScanRacy.tla LeanScanRacyProof.tla $C; cd $C
  python3 - "$m" <<'PY'
import sys
s=open('LeanScanRacyProof.tla').read(); a='  /\\ %s = TRUE' % sys.argv[1]
assert s.count(a)==1
open('LeanScanRacyProof.tla','w').write(s.replace(a,'  /\\ %s = FALSE' % sys.argv[1]))
PY
  { echo "CONTROL $m = FALSE"; nice -n 10 $T $OPTS LeanScanRacyProof.tla 2>&1; echo "rc=$?"; } > $D/ctl-$m.out 2>&1
  cd $D
done
echo DONE > $D/final.done
