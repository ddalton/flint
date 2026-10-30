#!/usr/bin/env bash
# leg-compare.sh <pv> <outdir> -- do the volume's legs hold the same bytes?
# The evidence a lost acked write needs before anything else touches the
# volume: the harness workload is stopped (pg-load, witness, pg scaled to 0,
# so the raid is torn down and no write is in flight), then each leg's head
# is snapshotted on its own node, exported on 127.0.0.1, read through a
# kernel connect from that node's csi-driver container and hashed 1 MiB at a
# time. For up to 16 differing chunks every leg's 4 KiB blocks are hashed and
# their first 32 bytes kept, so the difference can be read after the
# cluster is gone. The snapshots and exports are removed at the end; the
# workload is NOT restarted (the caller redeploys the harness).
#   <outdir>/legs.txt         index node lvol sync_state (the PV's record)
#   <outdir>/chunks-<i>.txt   "<chunk> <md5>" per 1 MiB of leg i
#   <outdir>/blocks-<i>.txt   "<chunk>.<block> <md5> <first 32 bytes hex>"
#   <outdir>/diff.txt         last line: "chunks compared N; differing D; ..."
set -uo pipefail
cd "$(dirname "$0")"
. ./lib.sh
PV=${1:?pv}; OUT=${2:?outdir}
need_env
mkdir -p "$OUT"
PORT=4433 T=$(date -u +%H%M%S)
log() { echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) leg-compare: $*" | tee -a "$OUT/log"; }
tgt() { kubectl exec -n "$DRIVER_NS" "$(csi_node_pod "$1")" -c spdk-tgt -- \
  python3 /usr/local/scripts/rpc.py -s /var/tmp/spdk.sock "${@:2}" < /dev/null; }
drv() { kubectl exec -n "$DRIVER_NS" "$(csi_node_pod "$1")" -c flint-csi-driver -- bash -c "$2" _ "${@:3}" < /dev/null; }
CONNECT='nqn=$1; hid=$(cat /proc/sys/kernel/random/uuid)
nvme connect -t tcp -a 127.0.0.1 -s '"$PORT"' -n "$nqn" --hostnqn="nqn.2014-08.org.nvmexpress:uuid:$hid" --hostid="$hid" >/tmp/lc.err 2>&1 \
  || { echo "ERR connect $(tail -1 /tmp/lc.err)"; exit 10; }
dev=; for i in $(seq 1 100); do for b in /sys/block/nvme*; do n=${b##*/}; case $n in *c*) continue ;; esac
  [ "$(cat $b/device/subsysnqn 2>/dev/null)" = "$nqn" ] && dev=/dev/$n && break; done
  [ -n "$dev" ] && [ -b "$dev" ] && break; dev=; sleep 0.1; done
[ -n "$dev" ] || { nvme disconnect -n "$nqn" >/dev/null 2>&1; echo "ERR no device"; exit 11; }'

VH=$(kubectl get pv "$PV" -o jsonpath='{.spec.csi.volumeHandle}')
[ -n "$VH" ] || { log "no PV $PV"; exit 2; }
log "stopping the workload on $PV"
for w in deploy/pg-load deploy/witness statefulset/pg; do kubectl scale -n "$NS" "$w" --replicas=0 >/dev/null 2>&1; done
for i in $(seq 1 120); do [ -z "$(va_for_pv "$PV")" ] && break; sleep 5; done
[ -z "$(va_for_pv "$PV")" ] || { log "the volume is still attached after 10 min; no compare"; exit 3; }
sleep 10
kubectl get pv "$PV" -o json | python3 -c '
import json, sys
pv = json.load(sys.stdin); a = pv["metadata"].get("annotations") or {}
rec = json.loads(a.get("disk.chert.us/replica-sync-state") or "null")
for i, r in enumerate(rec["replicas"] if rec else []):
    print(i, r["node_name"], r.get("active_lvol_uuid") or r["lvol_uuid"], r["sync_state"])' > "$OUT/legs.txt"
log "legs: $(tr '\n' ';' < "$OUT/legs.txt")"
[ -s "$OUT/legs.txt" ] || { log "no sync record on $PV; no compare"; exit 4; }

head_of() {  # <i> <node> <lvol>: the lvol as recorded, else the bdev behind leg i's export
  if tgt "$2" bdev_get_bdevs -b "$3" >/dev/null 2>&1; then echo "$3"; return; fi
  tgt "$2" nvmf_get_subsystems 2>/dev/null | python3 -c '
import json, sys
want = sys.argv[1]
for s in json.load(sys.stdin):
    if s["nqn"].endswith(want):
        for ns in s.get("namespaces") or []:
            print(ns["bdev_name"]); sys.exit(0)' ":volume:${VH}_$1"
}
export_snap() {  # <i> <node> <lvol>: snapshot + export; prints "<snapshot> <nqn>"
  local h s q
  h=$(head_of "$1" "$2" "$3")
  [ -n "$h" ] || { echo "ERR no head for leg $1 ($3)"; return 1; }
  s=$(tgt "$2" bdev_lvol_snapshot "$h" "legcmp-$T-$1" 2>>"$OUT/rpc.err") || { echo "ERR snapshot of $h"; return 1; }
  s=$(echo "$s" | tr -d '"[:space:]')
  q="nqn.2024-11.com.flint:legcmp:$T-$1"
  { tgt "$2" nvmf_create_subsystem "$q" -a -s "LC$1$T" && tgt "$2" nvmf_subsystem_add_ns "$q" "$s" \
      && tgt "$2" nvmf_subsystem_add_listener "$q" -t tcp -a 127.0.0.1 -s $PORT -f ipv4; } >>"$OUT/rpc.err" 2>&1 \
    || { echo "ERR export of $s"; tgt "$2" bdev_lvol_delete "$s" >/dev/null 2>&1; return 1; }
  echo "$s $q"
}

# phase 1: every leg, every 1 MiB chunk (the legs in parallel)
while read -r i n u st; do
  ( r=$(export_snap "$i" "$n" "$u") || { echo "$r" > "$OUT/chunks-$i.txt"; exit 1; }
    echo "$n $r" > "$OUT/export-$i.txt"
    drv "$n" "$CONNECT"'
sz=$(blockdev --getsize64 $dev); n=$((sz / 1048576)); echo "SIZE $sz"
c=0; while [ $c -lt $n ]; do echo "$c $(dd if=$dev bs=1M skip=$c count=1 iflag=direct status=none | md5sum | cut -c1-16)"; c=$((c + 1)); done
nvme disconnect -n "$nqn" >/dev/null 2>&1' "${r#* }" > "$OUT/chunks-$i.txt" 2>&1
  ) &
done < "$OUT/legs.txt"
wait
python3 - "$OUT" > "$OUT/diff.txt" <<'PY'
import collections, glob, sys
o = sys.argv[1]; legs = {}
for f in sorted(glob.glob(o + "/chunks-*.txt")):
    i = int(f.rsplit("-", 1)[1].split(".")[0]); m = {}
    for line in open(f):
        p = line.split()
        if len(p) == 2 and p[0].isdigit(): m[int(p[0])] = p[1]
    legs[i] = m
ids = sorted(legs)
n = min((len(m) for m in legs.values()), default=0)
diff = [c for c in range(n) if len({legs[i].get(c) for i in ids}) > 1]
odd = collections.Counter()
for c in diff:
    v = [legs[i].get(c) for i in ids]
    for k, i in enumerate(ids):
        if v.count(v[k]) == 1 and len(ids) > 2 and v.count(v[(k + 1) % len(ids)]) == len(ids) - 1: odd[i] += 1
nz = {i: sum(1 for h in legs[i].values() if h != "b6d81b360a5672d8") for i in ids}
open(o + "/differing.txt", "w").write("\n".join(map(str, diff[:16])) + "\n")
print("legs %s; non-zero chunks per leg %s" % (ids, nz))
print("chunks compared %d (1 MiB); differing %d; odd-leg-out counts %s; first differing %s"
      % (n, len(diff), dict(odd), diff[:20]))
PY
log "$(tail -1 "$OUT/diff.txt")"

# phase 2: the differing chunks, block by block, on every leg
if [ -s "$OUT/differing.txt" ] && [ -n "$(tr -d '\n' < "$OUT/differing.txt")" ]; then
  while read -r i n u st; do
    read -r _ s q < "$OUT/export-$i.txt" 2>/dev/null || continue
    drv "$n" "$CONNECT"'
shift; for c in "$@"; do b=0; while [ $b -lt 256 ]; do off=$((c * 256 + b))
  printf "%s.%s %s %s\n" $c $b "$(dd if=$dev bs=4096 skip=$off count=1 iflag=direct status=none | md5sum | cut -c1-16)" \
    "$(dd if=$dev bs=4096 skip=$off count=1 iflag=direct status=none | head -c 32 | od -An -tx1 | tr -d " \n")"
  b=$((b + 1)); done; done
nvme disconnect -n "$nqn" >/dev/null 2>&1' "$q" $(cat "$OUT/differing.txt") > "$OUT/blocks-$i.txt" 2>&1 &
  done < "$OUT/legs.txt"
  wait
  log "block detail for $(grep -c . "$OUT/differing.txt") differing chunks in blocks-<leg>.txt"
fi

# cleanup: exports and snapshots (the workload stays down)
for f in "$OUT"/export-*.txt; do
  [ -f "$f" ] || continue
  read -r n s q < "$f"
  tgt "$n" nvmf_delete_subsystem "$q" >/dev/null 2>&1
  tgt "$n" bdev_lvol_delete "$s" >/dev/null 2>&1 || log "snapshot $s on $n not deleted"
done
log "done"
