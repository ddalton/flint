# kill3: pk9 repeats pk8's Id17 and Z4Ack17 (valid in pk8: they do not read FlintCatchupSInd.tla); stop pk9's copies
sleep 90
for a in Id17 Z4Ack17; do for p in $(ps -eo pid,args | grep "[/]work/cache-pk9-$a/" | awk '{print $1}'); do kill $p; echo "killed pk9 $a $p" >> $OUT/kill.txt; done; done
free -g >> $OUT/kill.txt
