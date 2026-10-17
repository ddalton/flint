#!/bin/bash
# The part-parallelism measurement (2026-10-17): one pack uploaded through
# forge's own path (examples/upload_bench.rs = packio::upload_file, the store
# built as the syncer builds it) to a real bucket, per arm, on this instance.
# ONE script: packs, every arm, cleanup, results to the bucket, shutdown.
#
#   run.sh <bucket> <tag>      (userdata.sh runs it; <tag> names the instance type)
#
# Arms: the gate counts each 64 MiB part, so a gate of 256 MiB admits 4 parts
# whatever the width; C and D size it to the width, E is lean's shipped pair.
#   A  1 part,   no gate     today's binary: the control
#   B  4 parts,  256 MiB
#   C  8 parts,  512 MiB
#   D 16 parts, 1024 MiB
#   E  8 parts,  256 MiB     lean's flint_sync defaults (clamped to 4 by the gate)
# Each round runs the arms in an order rotated by the round, so no arm always
# goes first. The packs are read once before the first run (git has just
# written a pack when the syncer uploads it, so a warm page cache is the real
# case) and stay cached: 14 GiB of packs on 32+ GiB instances.
set -u
BK=$1
TAG=$2
W=/data
OUT=$W/out
B=$W/upload_bench
mkdir -p $OUT
export FLINT_FORGE_BUCKET=$BK AWS_REGION=us-west-1 AWS_DEFAULT_REGION=us-west-1
log() { echo "$(date -u +%FT%TZ) $*" >> $OUT/run.log; }
push() { aws s3 sync $OUT s3://$BK/out/$TAG --quiet; }

chmod +x $B
{ date -u +%FT%TZ; nproc; free -g; lsblk; md5sum $B
  curl -s -H "X-aws-ec2-metadata-token: $(curl -s -X PUT http://169.254.169.254/latest/api/token -H 'X-aws-ec2-metadata-token-ttl-seconds: 60')" \
    http://169.254.169.254/latest/meta-data/instance-type; echo; } > $OUT/setup.txt 2>&1
for g in 4 10; do
  log "pack ${g} GiB"
  head -c ${g}G /dev/urandom > $W/pack$g
done
cat $W/pack4 $W/pack10 > /dev/null
log "packs ready: $(ls -l $W/pack4 $W/pack10 | awk '{print $5}' | tr '\n' ' ')"
push

ARMS=("A 1 0" "B 4 256" "C 8 512" "D 16 1024" "E 8 256")
for r in 1 2 3; do
  for size in 4 10; do
    for k in 0 1 2 3 4; do
      read -r name par mb <<< "${ARMS[$(( (k + r) % 5 ))]}"
      key=bench/$TAG/r$r-${size}g-$name
      log "start round=$r size=${size}g arm=$name"
      FLINT_FORGE_UPLOAD_PART_PARALLELISM=$par FLINT_FORGE_UPLOAD_INFLIGHT_MB=$mb \
        /usr/bin/time -f "%e %M" -o $OUT/time.tmp \
        $B $W/pack$size $key > $OUT/line.tmp 2> $OUT/err.tmp &
      bp=$!
      # The width AS IT RAN: the bench's established :443 connections, sampled
      # each second. An arm that shows no gain with a peak of 1 never widened.
      peak=0
      while kill -0 $bp 2>/dev/null; do
        c=$(ss -tnp state established '( dport = :443 )' 2>/dev/null | grep -c upload_bench)
        [ "$c" -gt "$peak" ] && peak=$c
        sleep 1
      done
      wait $bp
      rc=$?
      read -r wall rss < $OUT/time.tmp
      echo "RES tag=$TAG round=$r size_gib=$size arm=$name $(cat $OUT/line.tmp) wall_s=$wall maxrss_kb=$rss conn_peak=$peak rc=$rc" >> $OUT/results.txt
      [ $rc -ne 0 ] && { echo "--- $name r$r ${size}g"; cat $OUT/err.tmp; } >> $OUT/errors.txt
      aws s3 rm s3://$BK/$key --quiet
      push
    done
  done
done
log "ALL-DONE"
echo "ALL-DONE" >> $OUT/results.txt
push
shutdown -h +1
