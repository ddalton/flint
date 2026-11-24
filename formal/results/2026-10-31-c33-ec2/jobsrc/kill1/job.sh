# kill1: stop pk5 and pk6 (superseded by pk7) to free memory on c33b
for j in pk5 pk6; do
  for p in $(ps -eo pid,args | grep "[/]work/cache-$j-" | awk '{print $1}'); do kill $p; echo "killed $j checker $p" >> $OUT/kill.txt; done
  for p in $(ps -eo pid,args | grep "[b]ash job.sh" | awk '{print $1}'); do [ "$(readlink /proc/$p/cwd)" = /work/jobs/$j ] && { kill $p; echo "killed $j job.sh $p" >> $OUT/kill.txt; }; done
done
sleep 5; free -g >> $OUT/kill.txt
