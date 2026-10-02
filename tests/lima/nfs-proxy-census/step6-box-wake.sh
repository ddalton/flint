#!/bin/bash
# nfs-proxy step 6, phase B on the box (docs/plans/flint-lite-nfs-proxy-step6-rig-plan.md):
# how long a WAKE takes, from suspend (pod start, disk kept) and from
# hibernate (CR alone: new disk, import from the bucket), at 1k / 5k / 10k
# files. Each wake is measured twice:
#   client: first byte of one file, then a full `find` count, on a FRESH
#           mount (so nothing is cached). A hard mount retries DELAY with a
#           backoff up to 15 s, so this can exceed the server's wake by up
#           to that much — which is also what a user sees.
#   server: the woken pod's creation to Ready (import included).
# RustFS on the same box: a LOWER bound on a real S3 import.
#   bash step6-box-wake.sh        # REPS=3 TIERS="1000 5000 10000"; KEEP=1
source "$(cd "$(dirname "$0")" && pwd)/rig-safety.sh"
set -u
CLUSTER=flint-step6b; OUT=$HOME/nfs-proxy-step6-B; MNT=/mnt/px6b
cd "$(dirname "$0")" && source ./step6-lib.sh
TIERS=${TIERS:-"1000 5000 10000"}; REPS=${REPS:-3}
HUBS=""; for n in $TIERS; do HUBS="$HUBS w$n"; done

echo "== bring-up"
up_cluster; up_operator
shares $HUBS; wait_ready $HUBS
K -n $OPNS rollout status deploy/flint-lite-operator-nfs-proxy --timeout=180s >/dev/null || exit 1
mount_proxy $HUBS || exit 1
for h in $HUBS; do seed $MNT/$h ${h#w} & done; wait
for h in $HUBS; do
  c=$(sudo find $MNT/$h -type f | wc -l)
  check "$h holds ${h#w} files" '[ "$c" = "${h#w}" ]'
done
for _ in $(seq 1 60); do n=$(for h in $HUBS; do rpo $h; done | grep -c True); [ "$n" = $(echo $HUBS | wc -w) ] && break; sleep 10; done
check "every hub is flushed (rpoClean)" '[ "$n" = $(echo $HUBS | wc -w) ]'
unmount_hard $MNT

ann() { K -n $NS annotate flintshare $1 --overwrite chert.us/idle-state=$2 chert.us/idle-since=$(date -u +%Y-%m-%dT%H:%M:%SZ) >/dev/null; }
objs() { K -n $NS get deploy,svc,cm,pvc -l chert.us/share=$1 -o name 2>/dev/null | wc -l; }
ready_s() {  # the current pod's creation to Ready, in seconds
  K -n $NS get pod $(pod $1) -o json 2>/dev/null | python3 -c '
import json, sys
from datetime import datetime
p = json.load(sys.stdin)
f = lambda t: datetime.fromisoformat(t.replace("Z", "+00:00"))
r = [c for c in p["status"]["conditions"] if c["type"] == "Ready" and c["status"] == "True"]
if r:
    print("%.0f" % (f(r[0]["lastTransitionTime"]) - f(p["metadata"]["creationTimestamp"])).total_seconds())
else:
    print("?")'
}
measure() {  # $1 hub, $2 kind, $3 rep
  local h=$1 n=${1#w}
  mount_proxy $HUBS >/dev/null || { echo "$2 $h rep=$3 MOUNT-FAILED"; return; }
  local t0 t1 t2 got
  t0=$(date +%s.%N)
  sudo timeout 900 cat $MNT/$h/d000/f00000 > /dev/null; local rc=$?
  t1=$(date +%s.%N)
  got=$(sudo timeout 900 find $MNT/$h -type f | wc -l)
  t2=$(date +%s.%N)
  for _ in $(seq 1 60); do [ "$(phase $h)" = Ready ] && break; sleep 2; done
  printf "%s\t%s\ttier=%s\trep=%s\trc=%s\tfiles=%s\tclient_first_byte_s=%.1f\tclient_full_list_s=%.1f\tserver_pod_ready_s=%s\n" \
    $2 $h $n $3 $rc $got $(echo "$t1 - $t0" | bc) $(echo "$t2 - $t0" | bc) "$(ready_s $h)"
  unmount_hard $MNT
}

echo "== wakes from SUSPEND (disk kept)"
for r in $(seq 1 $REPS); do
  for h in $HUBS; do
    ann $h Suspended
    for _ in $(seq 1 90); do [ -z "$(pod $h)" ] && break; sleep 2; done
    measure $h suspend $r
  done
done | tee $OUT/suspend.tsv

echo "== wakes from HIBERNATE (CR alone: new disk, bucket import)"
for r in $(seq 1 $REPS); do
  for h in $HUBS; do
    sid0=$(st $h serverId)
    ann $h HibernateVerifying
    # verify waits a full hub lease of uptime (restored leases), then drains
    for _ in $(seq 1 300); do [ "$(phase $h)" = Hibernated ] && [ "$(objs $h)" = 0 ] && break; sleep 3; done
    [ "$(objs $h)" = 0 ] || { echo "hibernate $h rep=$r NOT-PARKED phase=$(phase $h) objs=$(objs $h)"; continue; }
    measure $h hibernate $r
    sid1=$(st $h serverId)
    [ "$sid1" != "$sid0" ] || echo "hibernate $h rep=$r WARNING: serverId unchanged ($sid0) — was it really a new disk?"
  done
done | tee $OUT/hibernate.tsv

for f in suspend hibernate; do
  n=$(grep -c "files=" $OUT/$f.tsv); full=$(awk -F'\t' '{split($6,a,"="); t=substr($3,6); if (a[2]==t) c++} END {print c+0}' $OUT/$f.tsv)
  check "$f: $n wakes, every one served all its files ($full/$n)" '[ "$n" -gt 0 ] && [ "$full" = "$n" ]'
done
echo "== summary (ranges over reps)"
python3 - $OUT/suspend.tsv $OUT/hibernate.tsv <<'P' | tee $OUT/summary.txt
import sys
for path in sys.argv[1:]:
    rows = {}
    for line in open(path):
        f = line.rstrip("\n").split("\t")
        if len(f) < 9: continue
        kv = dict(x.split("=") for x in f[2:])
        rows.setdefault(int(kv["tier"]), []).append(kv)
    for tier in sorted(rows):
        def rng(k):
            v = [float(r[k]) for r in rows[tier] if r[k] not in ("?", "")]
            return f"{min(v):.0f}-{max(v):.0f}s" if v else "?"
        print(f"{path.split('/')[-1][:-4]:9} tier {tier:5}  first byte {rng('client_first_byte_s'):>9}  full list {rng('client_full_list_s'):>9}  server ready {rng('server_pod_ready_s'):>9}")
P
echo "RESULT: $ok passed, $bad failed"
