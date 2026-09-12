#!/usr/bin/env bash
# In-VM: sdk vs raw reads against loopback fakes3, one binary. usage: vm-raw-ab.sh <reps> <fanouts...>
set -uo pipefail
BIN=/var/tmp/flint-sync-raw; EP=http://127.0.0.1:9000
export AWS_ACCESS_KEY_ID=x AWS_SECRET_ACCESS_KEY=y AWS_REGION=us-west-1 AWS_EC2_METADATA_DISABLED=true
export FLINT_SYNC_BUCKET=bucket FLINT_SYNC_PREFIX=p FLINT_SYNC_ENDPOINT=$EP
reps=$1; shift
run() { # <rep> <arm> <fanout>
  local r=$1 arm=$2 f=$3 d=/dev/shm/co raw=false; [ "$arm" = raw ] && raw=true
  rm -rf $d; mkdir -p $d
  local st0; st0=$(curl -s $EP/__stats | awk '/bytes_out|^reqs/{printf "%s ", $2}')
  ( TIMEFORMAT='%U %S'; time FLINT_SYNC_ROOT=$d FLINT_SYNC_FANOUT=$f FLINT_SYNC_RAW_READS=$raw $BIN checkout > /var/tmp/run.out 2> /var/tmp/run.err ) 2> /var/tmp/run.time &
  local sub=$! last="" s; sleep 0.05; local pid; pid=$(pgrep -n -x flint-sync-raw || echo $sub)
  while kill -0 $sub 2>/dev/null; do s=$(for t in /proc/$pid/task/*/stat; do awk '{print $1, $14+$15}' "$t" 2>/dev/null; done); [ -n "$s" ] && last="$s"; sleep 0.1; done
  wait $sub; local rc=$?
  local exact; exact=$(awk '{printf "%.2f", $1+$2}' /var/tmp/run.time)
  local phase; phase=$(grep -F 'flint-sync: phase' /var/tmp/run.err | head -1)
  local fs mat; fs=$(echo "$phase" | sed -n 's/.*fetch=\([0-9.]*\)s.*/\1/p'); mat=$(sed -n 's/.*— \([0-9]*\) materialized.*/\1/p' /var/tmp/run.err | head -1)
  local st1; st1=$(curl -s $EP/__stats | awk '/bytes_out|^reqs/{printf "%s ", $2}')
  echo "$last" | awk -v pid=$pid -v r=$r -v arm=$arm -v f=$f -v fs="$fs" -v mat="$mat" -v rc=$rc -v ex="$exact" -v srv="$(echo $st0 $st1 | awk '{print $3-$1, $4-$2}')" '{c=$2/100; if($1==pid) m=c; else o+=c} END{printf "rep=%s arm=%s fanout=%s rc=%s files/s=%d cpu/file=%.0fus cpu_exact=%ss fetch=%ss materialized=%s sampled_main=%.2fs sampled_others=%.2fs server(bytes reqs)=%s\n", r, arm, f, rc, (fs>0?mat/fs:0), (mat>0?ex/mat*1e6:0), ex, fs, mat, m, o, srv}'
  [ "$rc" -ne 0 ] && tail -n 2 /var/tmp/run.err
}
ident() { # <arm>
  run i "$1" 256 | sed 's/^/identity: /'
  (cd /dev/shm/co && find . -type f -not -path './.flint*' | sort | xargs sha256sum) > /var/tmp/co.sha256
  echo "   arm=$1 files_on_disk=$(wc -l < /var/tmp/co.sha256) sha256_diff_lines=$(diff /var/tmp/corpus.sha256 /var/tmp/co.sha256 | wc -l)"
}
for arm in sdk raw; do ident $arm; done
for r in $(seq 1 $reps); do for f in "$@"; do for arm in sdk raw; do run $r $arm $f; done; done; done
