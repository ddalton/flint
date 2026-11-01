#!/bin/bash
# isa-rec.sh (box, 11-01): the two stamp-proof steps only Isabelle proves (Isabelle fails on the EC2 instances),
# on FlintCatchupSProof.tla 9ed5580a, fresh cache, stretch 5; every input's md5 ASSERTED before tlapm runs.
D=/mnt/nvme/sproof-2026-10-31; TL=/mnt/nvme/tlaps/unpack/tlapm/bin/tlapm; O=$D/rec/RECORD.txt
m() { md5sum < $1 | cut -c1-8; }
cd $D/rec || exit 1
echo "isa-rec start $(date -u +%FT%TZ) host $(hostname) tlapm $($TL --version 2>&1 | head -1) ($TL)" > $O
for p in "FlintCatchupSProof.tla 9ed5580a" "FlintCatchupSInd.tla c70f9660" "FlintCatchupS.tla 61ae6829"; do
  set -- $p; got=$(m $1); echo "md5 $1 $got (want $2)" >> $O
  [ "$got" = "$2" ] || { echo "MD5 MISMATCH $1" >> $O; exit 1; }
done
for r in "minprop 122 139" "readat 1250 1263"; do
  set -- $r; mkdir -p $1; cp FlintCatchupS.tla FlintCatchupSInd.tla FlintCatchupSProof.tla $1/
  echo "run $1 md5 $(m $1/FlintCatchupSProof.tla) $(m $1/FlintCatchupSInd.tla) $(m $1/FlintCatchupS.tla) range $2 $3 $(date -u +%TZ)" >> $O
done
t0=$(date +%s)
(cd minprop && nice -n 10 $TL --toolbox 122 139 --cleanfp --stretch 5 --threads 2 FlintCatchupSProof.tla > out.txt 2>&1; echo "minprop rc=$? secs=$(( $(date +%s) - t0 ))" > rc.txt) & p1=$!
(cd readat && nice -n 10 $TL --toolbox 1250 1263 --cleanfp --stretch 5 --threads 2 FlintCatchupSProof.tla > out.txt 2>&1; echo "readat rc=$? secs=$(( $(date +%s) - t0 ))" > rc.txt) & p2=$!
wait $p1 $p2
for d in minprop readat; do
  echo "$(cat $d/rc.txt) | $(grep -E 'All [0-9]+ obligations? proved|obligations? failed' $d/out.txt | head -1) | isabelle: $(awk '/@!!BEGIN/{b=""} {b=b"\n"$0} /@!!END/{ if (b ~ /prover:isabelle/) print b }' $d/out.txt | grep -E '@!!(loc|status|meth)' | paste -s -d' ' -)" >> $O
done
echo "isa-rec done $(date -u +%FT%TZ)" >> $O
