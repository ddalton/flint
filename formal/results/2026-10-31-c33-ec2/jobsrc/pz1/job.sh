# pz1: inv-zombie's 4-replica trace probe (rev 16), bounded at 3,000 s, 24 workers
md5() { md5sum < $1 | cut -c1-8; }
O=$OUT/pz1.txt; echo "S $(md5 FlintCatchupS.tla) cfg $(md5 FlintCatchupZ4Ack.cfg) $($MC -version 2>&1 | head -1) compiled 24 workers $(date -u +%TZ)" > $O
t0=$(date +%s)
timeout 3000 env TLAMC_CACHE=$W/cache-pz1 $MC -compile -workers 24 -fpmem 8192 -queue-mem 8192 -metadir $W/meta-pz1 -config FlintCatchupZ4Ack.cfg FlintCatchupS.tla > $OUT/Z4Ack.out 2>&1
rc=$?; rm -rf $W/meta-pz1
got=$(grep -m1 -oE "No error has been found|Invariant [A-Za-z_0-9]+ is violated|[Ee]rror:.*" $OUT/Z4Ack.out | head -1)
echo "RESULT rc=$rc got=[$got] $(grep -oE "[0-9]+ distinct states found\. Depth [0-9]+" $OUT/Z4Ack.out | tail -1) $(grep -oE "depth [0-9]+, [0-9]+ generated, [0-9]+ distinct" $OUT/Z4Ack.out | tail -1) $(( $(date +%s) - t0 ))s" >> $O
