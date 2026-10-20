#!/bin/bash
# LeanScanRacyProof: the RECORD run (--cleanfp), then one CONTROL per rule
# detail -- its Shipped line set FALSE must fail the step that relies on it.
# Usage: run-racy.sh <md5 of LeanScanRacy.tla> <md5 of LeanScanRacyProof.tla>
T=/mnt/nvme/tlaps/unpack/tlapm/bin/tlapm; D=/mnt/nvme/lean-racy-proof
cd $D || exit 1
echo "$1  LeanScanRacy.tla
$2  LeanScanRacyProof.tla" | md5sum -c || exit 1
OPTS="--toolbox 0 0 --cleanfp --stretch 4 --threads 1"
{ echo "RECORD $(date -u +%Y-%m-%dT%H:%M:%SZ) tlapm $($T --version 2>&1 | head -1)"
  md5sum LeanScanRacy.tla LeanScanRacyProof.tla
  echo "cmd: tlapm $OPTS LeanScanRacyProof.tla"
  /usr/bin/time -v nice -n 10 $T $OPTS LeanScanRacyProof.tla 2>&1
  echo "rc=$?"; } > record.out 2>&1
for m in RacyClean MeasureAtRead ClearOutsideWindow; do
  C=$D/ctl-$m; rm -rf $C; mkdir -p $C; cp LeanScanRacy.tla LeanScanRacyProof.tla $C; cd $C
  python3 - "$m" <<'PY'
import sys
s=open('LeanScanRacyProof.tla').read(); a='  /\\ %s = TRUE' % sys.argv[1]
assert s.count(a)==1
open('LeanScanRacyProof.tla','w').write(s.replace(a,'  /\\ %s = FALSE' % sys.argv[1]))
PY
  { echo "CONTROL $m = FALSE $(date -u +%Y-%m-%dT%H:%M:%SZ)"; md5sum *.tla
    grep -n -A3 "^ASSUME Shipped" LeanScanRacyProof.tla
    nice -n 10 $T $OPTS LeanScanRacyProof.tla 2>&1; echo "rc=$?"; } > $D/ctl-$m.out 2>&1
  cd $D
done
echo DONE > $D/final.done
