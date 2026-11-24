md5() { md5sum < $1 | cut -c1-8; }
SEED=
[ -n "$SEED" ] && [ -d "$SEED/.tlacache" ] && cp -r $SEED/.tlacache .
echo "S $(md5 FlintCatchupS.tla) ind $(md5 FlintCatchupSInd.tla) proof $(md5 FlintCatchupSProof.tla) threads 5 stretch 5 range 3307 3996 seed [$SEED] start $(date -u +%TZ)" > $OUT/summary.txt
t0=$(date +%s)
/usr/bin/time -v -o $OUT/time.txt $TL --toolbox 3307 3996 --cleanfp --stretch 5 --threads 5 FlintCatchupSProof.tla > $OUT/tlapm.out 2>&1
rc=$?
python3 $SUMM $OUT/tlapm.out >> $OUT/summary.txt
echo "DONE rc=$rc secs=$(( $(date +%s) - t0 )) maxrss=$(grep -oP "Maximum resident set size \(kbytes\): \K\d+" $OUT/time.txt)KB $(date -u +%TZ)" >> $OUT/summary.txt
