# kill2: stop pk4 and pk7 (rev 16, superseded by pk8 on rev 17) to free memory on c33b
for j in pk4 pk7; do
  for p in $(ps -eo pid,args | grep "[/]work/cache-$j-" | awk '{print $1}'); do kill $p; echo "killed $j checker $p" >> $OUT/kill.txt; done
  for p in $(ps -eo pid,args | grep "[b]ash job.sh" | awk '{print $1}'); do [ "$(readlink /proc/$p/cwd)" = /work/jobs/$j ] && { kill $p; echo "killed $j job.sh $p" >> $OUT/kill.txt; }; done
done
sleep 5; free -g >> $OUT/kill.txt
