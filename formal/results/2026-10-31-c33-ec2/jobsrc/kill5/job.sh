# kill5: stop pk10 (drain off; superseded by pk11, drain in) to free memory on c33b
for j in pk10; do
  for p in $(ps -eo pid,args | grep "[/]work/cache-$j-" | awk '{print $1}'); do kill $p; echo "killed $j checker $p" >> $OUT/kill.txt; done
  for p in $(ps -eo pid,args | grep "[b]ash job.sh" | awk '{print $1}'); do [ "$(readlink /proc/$p/cwd)" = /work/jobs/$j ] && { kill $p; echo "killed $j job.sh $p" >> $OUT/kill.txt; }; done
done
sleep 5; free -g >> $OUT/kill.txt
