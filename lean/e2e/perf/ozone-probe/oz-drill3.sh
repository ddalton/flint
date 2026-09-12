set -u
export HOME=/root
LOG=/mnt/nvme/drill3.log; : > $LOG
say() { echo "$*" | tee -a $LOG; }
export AWS_ACCESS_KEY_ID=flint AWS_SECRET_ACCESS_KEY=flintsecret AWS_REGION=us-east-1 AWS_DEFAULT_REGION=us-east-1 AWS_EC2_METADATA_DISABLED=true
S3G=http://localhost:9878; export FLINT_SYNC_ENDPOINT=$S3G FLINT_SYNC_BUCKET=lean
cg() { # cg <container> -> cpu usage_usec of its cgroup
  local id=$(docker inspect -f '{{.Id}}' "$1" 2>/dev/null); local f=$(ls /sys/fs/cgroup/system.slice/docker-$id.scope/cpu.stat 2>/dev/null || find /sys/fs/cgroup -path "*docker-$id*" -name cpu.stat 2>/dev/null | head -1); awk '/^usage_usec/{print $2}' "$f" 2>/dev/null || echo 0; }
snap() { for c in ozone-s3g-1 ozone-om-1 ozone-datanode-1 ozone-scm-1; do printf '%s=%s ' $c $(cg $c); done; echo; }
delta() { # delta "<before>" "<after>" -> per-container CPU seconds
  python3 - "$1" "$2" <<'PY'
import sys
b=dict(kv.split('=') for kv in sys.argv[1].split()); a=dict(kv.split('=') for kv in sys.argv[2].split())
print(' '.join(f"{k.replace('ozone-','').replace('-1','')}={(int(a[k])-int(b[k]))/1e6:.2f}s" for k in b))
PY
}
say "node: $(nproc) vCPU; containers: $(docker ps --format '{{.Names}}' | tr '\n' ' ')"
say "=== checkout of ws2 (2,000 x 8 KiB): who burns the CPU, by fanout ==="
say "fanout | run | files/s | syncer user+sys (us/file) | s3g | om | datanode | scm  (CPU seconds over the run)"
for fan in 32 128 512; do for r in 1 2; do
  E=/mnt/nvme/wsF; rm -rf $E; mkdir -p $E; export FLINT_SYNC_ROOT=$E FLINT_SYNC_PREFIX=ws2 FLINT_SYNC_FANOUT=$fan
  sync; echo 3 > /proc/sys/vm/drop_caches
  b=$(snap); TIMEFORMAT='%R %U %S'; t=$( { time flint-sync checkout > /mnt/nvme/co.out 2>&1; } 2>&1 ); a=$(snap)
  n=$(find $E/many -type f 2>/dev/null | wc -l)
  say "$(python3 -c "
w,u,s=map(float,'$t'.split()); n=$n
print(f'{$fan:>6} | {$r} | {n/w:7.0f} | {(u+s)/n*1e6:6.0f} | ', end='')") $(delta "$b" "$a") wall=${t%% *}s files=$n"
done; done
say "=== publish of 2,000 x 8 KiB (upload fanout default 32): who burns the CPU ==="
for r in 1 2; do
  G=/mnt/nvme/wsG; rm -rf $G; mkdir -p $G/many; export FLINT_SYNC_ROOT=$G FLINT_SYNC_PREFIX=ws3-$r; unset FLINT_SYNC_FANOUT
  flint-sync checkout > /dev/null 2>&1
  for i in $(seq 1 2000); do head -c 8192 /dev/urandom > $G/many/f$i.bin; done
  b=$(snap); TIMEFORMAT='%R %U %S'; t=$( { time flint-sync barrier > /mnt/nvme/bar.out 2>&1; } 2>&1 ); a=$(snap)
  say "publish run $r: $(python3 -c "
w,u,s=map(float,'$t'.split()); print(f'{2000/w:.0f} files/s, syncer {(u+s)/2000*1e6:.0f} us/file,', end='')") $(delta "$b" "$a") wall=${t%% *}s $(tail -1 /mnt/nvme/bar.out | cut -c1-80)"
done
say "=== one GET on the wire (headers Ozone returns) ==="
curl -s -D - -o /dev/null -H "Host: localhost:9878" "$S3G/lean/ws2/files/many/f1.bin" 2>/dev/null | head -12 | tee -a $LOG
unset AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY AWS_EC2_METADATA_DISABLED
aws s3 cp $LOG s3://flint-ozone-probe/out/drill3.log --quiet --region us-west-1 && echo "log uploaded"
