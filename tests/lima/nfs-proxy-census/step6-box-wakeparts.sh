#!/bin/bash
# Where a wake's time goes (phase B measured the totals: ~13 s from
# suspend, ~27 s from hibernate, ~12 s of the latter outside the pod).
# One hub, 1k files; REPS wakes from each state. Per wake, a timeline of
# offsets from the client's first access (t0):
#   the client's DELAY retries (the proxy logs every DELAY it answers),
#   the proxy's wake request, the operator's objects (creation), the
#   claim binding, pod scheduled / started / Ready (k8s events and pod
#   conditions, 1 s resolution), the hub's own log lines, and the client's
#   first byte.
#   bash step6-box-wakeparts.sh        # REPS=3; KEEP=1
source "$(cd "$(dirname "$0")" && pwd)/rig-safety.sh"
set -u
CLUSTER=flint-step6w; OUT=$HOME/nfs-proxy-step6-W; MNT=/mnt/px6w
cd "$(dirname "$0")" && source ./step6-lib.sh
REPS=${REPS:-3}; H=w1000
# The clients' default RPC timeout (60 s), not the rigs' timeo=50: the
# proxy now HOLDS a compound for a waking hub (wakeHoldSecs, 20 s), and
# a 5 s client timeout is not what a real mount runs with.
MOPTS=${MOPTS:-,timeo=600}

echo "== bring-up"
up_cluster; up_operator
shares $H; wait_ready $H
K -n $OPNS rollout status deploy/flint-lite-operator-nfs-proxy --timeout=180s >/dev/null || exit 1
mount_proxy $H || exit 1
seed $MNT/$H 1000
check "$H holds 1000 files" '[ "$(sudo find $MNT/$H -type f | wc -l)" = 1000 ]'
for _ in $(seq 1 60); do [ "$(rpo $H)" = True ] && break; sleep 10; done
check "$H is flushed (rpoClean)" '[ "$(rpo $H)" = True ]'
unmount_hard $MNT

# Every proxy and operator line for the whole run, not just the window
# each wake's timeline reads: a wake request BETWEEN reps (one landed
# during a hibernate verification and woke the share) is otherwise lost.
K -n $OPNS logs -f deploy/flint-lite-operator-nfs-proxy --timestamps > $OUT/proxy-all.log 2>&1 &
K -n $OPNS logs -f deploy/flint-lite-operator --timestamps > $OUT/operator-all.log 2>&1 &

ann() { K -n $NS annotate flintshare $H --overwrite chert.us/idle-state=$1 chert.us/idle-since=$(date -u +%Y-%m-%dT%H:%M:%SZ) >/dev/null; }
objs() { K -n $NS get deploy,svc,cm,pvc -l chert.us/share=$H -o name 2>/dev/null | wc -l; }
PX() { K -n $OPNS get pod -l app.kubernetes.io/name=flint-lite-operator-nfs-proxy -o jsonpath='{.items[0].metadata.name}'; }

timeline() {  # $1 kind, $2 rep, $3 t0 (epoch.ns), $4 t1 (first byte)
  local since; since=$(date -u -d @${3%.*} +%Y-%m-%dT%H:%M:%SZ)
  local d=$OUT/$1-$2; mkdir -p $d
  K -n $OPNS logs $(PX) --timestamps --since-time=$since > $d/proxy.log 2>&1
  K -n $NS get events -o json > $d/events.json
  K -n $NS get deploy,pvc,pod -l chert.us/share=$H -o json > $d/objs.json
  K -n $NS logs $(pod $H) --timestamps > $d/hub.log 2>&1
  K -n $NS get flintshare $H -o json --show-managed-fields > $d/share.json
  python3 - $d $3 $4 $H <<'P'
import json, re, sys
from datetime import datetime, timezone
d, t0, t1, h = sys.argv[1], float(sys.argv[2]), float(sys.argv[3]), sys.argv[4]
def ts(s):  # RFC 3339 with or without fraction -> epoch
    s = re.sub(r"(\.\d{6})\d*", r"\1", s.replace("Z", "+00:00"))
    return datetime.fromisoformat(s).timestamp()
ev = []
for line in open(f"{d}/proxy.log"):
    t = line.split(" ", 1)[0]
    if "wake requested" in line: ev.append((ts(t), "proxy: wake requested"))
    elif "Delay" in line and "→" in line: ev.append((ts(t), "proxy: DELAY answered (a client retry)"))
for o in json.load(open(f"{d}/objs.json"))["items"]:
    k = o["kind"]; ev.append((ts(o["metadata"]["creationTimestamp"]), f"{k} created"))
    if k == "Pod":
        for c in o["status"].get("conditions", []):
            if c["status"] == "True": ev.append((ts(c["lastTransitionTime"]), f"pod condition {c['type']}"))
        for c in o["status"].get("containerStatuses", []):
            st = c.get("state", {}).get("running", {}).get("startedAt")
            if st: ev.append((ts(st), "container started"))
for e in json.load(open(f"{d}/events.json"))["items"]:
    name = e["involvedObject"]["name"]
    if not name.startswith(h): continue
    t = e.get("eventTime") or e.get("firstTimestamp") or e["metadata"]["creationTimestamp"]
    if t and ts(t) >= t0 - 1: ev.append((ts(t), f"event {e['involvedObject']['kind']}/{e['reason']}"))
hub = [l for l in open(f"{d}/hub.log")]
if hub: ev.append((ts(hub[0].split(" ", 1)[0]), "hub: first log line"))
for l in hub:
    for pat, label in [("server id", "hub: server id"), ("import", "hub: import"), ("listening", "hub: listening"),
                       ("Listening", "hub: listening"), ("NFS server", "hub: NFS server")]:
        if pat in l: ev.append((ts(l.split(" ", 1)[0]), label + ": " + l.split(" ", 1)[1].strip()[:90])); break
for m in json.load(open(f"{d}/share.json"))["metadata"].get("managedFields", []):
    if m.get("subresource") == "status": ev.append((ts(m["time"]), f"share status written ({m['manager']})"))
ev.append((t1, "CLIENT FIRST BYTE"))
seen = set()
for t, what in sorted(ev):
    if t < t0 - 1 or (t, what) in seen: continue
    seen.add((t, what))
    print(f"  {t - t0:7.2f}s  {what}")
P
}

measure() {  # $1 kind, $2 rep
  mount_proxy $H >/dev/null || { echo "$1 rep=$2 MOUNT-FAILED"; return; }
  local t0 t1
  t0=$(date +%s.%N)
  sudo timeout 900 cat $MNT/$H/d000/f00000 > /dev/null
  t1=$(date +%s.%N)
  for _ in $(seq 1 60); do [ "$(phase $H)" = Ready ] && break; sleep 2; done
  sleep 3
  echo "== $1 rep=$2: first byte at $(echo "$t1 - $t0" | bc | cut -c1-5)s"
  timeline $1 $2 $t0 $t1
  unmount_hard $MNT
}

for r in $(seq 1 $REPS); do
  ann Suspended
  for _ in $(seq 1 90); do [ -z "$(pod $H)" ] && break; sleep 2; done
  sleep 5
  measure suspend $r
done | tee $OUT/suspend.txt

for r in $(seq 1 $REPS); do
  ann HibernateVerifying
  for _ in $(seq 1 300); do [ "$(phase $H)" = Hibernated ] && [ "$(objs)" = 0 ] && break; sleep 3; done
  [ "$(objs)" = 0 ] || {
    echo "hibernate rep=$r NOT-PARKED"
    K -n $NS get flintshare $H -o jsonpath='{.metadata.annotations}{"\n"}' | grep -o '"chert.us/[a-z-]*":"[^"]*"'
    K -n $NS get events --field-selector involvedObject.kind=FlintShare --sort-by=.metadata.creationTimestamp | tail -8
    continue; }
  sleep 5
  measure hibernate $r
done | tee $OUT/hibernate.txt
echo "RESULT: $ok passed, $bad failed"
