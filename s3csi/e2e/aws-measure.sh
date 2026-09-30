#!/usr/bin/env bash
# Two MEASUREMENTS on a real cluster, after run-s3csi.sh setup (the same
# rig, the same helpers, imported the way run-legs.sh does):
#
#   M1  the block cache's cost AS DEPLOYED: a read cold then warm through
#       a mount with no cache and through one with a 768 MiB cache named
#       on the CR (the sharing default of 2026-09-29, withdrawn by §11
#       step 1 once this measured it), n reps each, on TWO sets — `mid` (4 × 128 MiB,
#       fits the cache) and `big` (6 × 1 GiB, six times the cache, so the
#       warm read can only miss) — on the node's actual emptyDir disk (an
#       8 GiB gp3 root on the trove i4i.large nodes), which the 09-12 door
#       drill on host NVMe with a 60 GB cache never measured.
#   M2  a SHARED mounter under N concurrent readers of the same 6 GiB:
#       the worker's memory peak and whether it is OOM-killed, at the
#       plugin's two-thirds target (682 at 1Gi), at mount-s3's own 95%
#       default (973 — every mount before 2026-09-30), and at a 4Gi
#       workers.sharedResources limit (2730).
#
#   KUBECONFIG=… CTX=s3a STORE=s3 NODE_EXEC=nodesh TAG=… BUCKET=… S3_KEY_FILE=… \
#       ./aws-measure.sh [reps] [readers]
#
# Numbers, not verdicts: every cell is printed and written to
# results/measure-<date>.tsv. STORE=minio runs too, but then the store
# is one pod on one node and the numbers say more about it than about
# the cache — the header says which.
set -u
cd "$(dirname "$0")"
REPS=${1:-3}
READERS=${2:-4}
REPO=$(cd ../.. && pwd)
eval "$(sed -n '/^CTX=\${CTX:-/,/^# ── setup \/ teardown/p' run-s3csi.sh | sed '$d')"
OUT="results/measure-$(date -u +%Y%m%d-%H%M%S).tsv"
mkdir -p results
row() { printf '%s\n' "$*" | tee -a "$OUT"; }
FILES="f1 f2 f3 f4 f5 f6"
MID_FILES="m1 m2 m3 m4"

# ── the sets ──────────────────────────────────────────────────────────
seed() { # <prefix> <bytes> <names…>
    local pre=$1 bytes=$2 f; shift 2
    for f in "$@"; do
        mcx mc stat "m/$BUCKET/$pre/$f" >/dev/null 2>&1 && continue
        mcx sh -c "head -c $bytes /dev/urandom | mc pipe m/$BUCKET/$pre/$f" >/dev/null 2>&1 && echo "  seeded $pre/$f" || { echo "  seeding $pre/$f FAILED" >&2; exit 1; }
    done
}
echo "── seeding big/ (6 × 1 GiB) and mid/ (4 × 128 MiB) if absent — store=$STORE bucket=$BUCKET"
seed big 1073741824 $FILES
seed mid 134217728 $MID_FILES
mcx mc ls m/$BUCKET/big/ m/$BUCKET/mid/ | sed 's/^/  /'
apply_fx measure-tenants.yaml >/dev/null
echo "── node $NODE: where an emptyDir lives"
onnode "df -h /var/lib/kubelet | tail -1; lsblk -dno NAME,SIZE,MODEL 2>/dev/null | head -4" | sed 's/^/  /'
row "# measure $(date -u +%FT%TZ) ctx=$CTX store=$STORE node=$NODE tag=$TAG reps=$REPS readers=$READERS"

reader_pod() { # <name> <cr> [nodeName]
    cat <<YAML
apiVersion: v1
kind: Pod
metadata: { name: $1, namespace: $NS }
spec:
  serviceAccountName: trainer
  ${3:+nodeName: $3}
  securityContext: { runAsNonRoot: true, runAsUser: 1001, seccompProfile: { type: RuntimeDefault } }
  volumes:
    - name: data
      csi: { driver: s3.csi.chert.us, volumeAttributes: { chert.us/mount: $2 } }
  containers:
    - name: agent
      image: busybox:1.36
      command: ["/bin/sh", "-c"]
      args: ["trap 'exit 0' TERM INT; sleep 86400 & wait"]
      securityContext: { allowPrivilegeEscalation: false, capabilities: { drop: [ALL] } }
      volumeMounts: [{ name: data, mountPath: /mnt/big }]
YAML
}
# Read the set's files in parallel inside the pod; wall time in ms from
# /proc/uptime (busybox date has no %N). Prints ms, or FAIL.
read_all() { # <pod> [files]
    local files=${2:-$FILES}
    $K -n $NS exec "$1" -c agent -- /bin/sh -c '
t0=$(cut -d" " -f1 /proc/uptime); rc=0
for f in '"$files"'; do cat /mnt/big/$f > /dev/null || rc=1 & done; wait
for f in '"$files"'; do [ -r /mnt/big/$f ] || rc=1; done
t1=$(cut -d" " -f1 /proc/uptime)
[ $rc = 0 ] && awk -v a=$t0 -v b=$t1 "BEGIN{printf \"%d\", (b-a)*1000}" || echo FAIL' 2>/dev/null
}
worker_peak_mib() { # <worker>
    $K -n $WNS exec "$1" -- sh -c 'p=$(cat /sys/fs/cgroup/memory.peak 2>/dev/null || cat /sys/fs/cgroup/memory/memory.max_usage_in_bytes 2>/dev/null); [ -n "$p" ] && echo $((p/1048576))' 2>/dev/null
}
worker_term() { # <worker> -> the container's last/current terminated reason, or -
    $K -n $WNS get pod "$1" -o jsonpath='{.status.containerStatuses[0].state.terminated.reason}{.status.containerStatuses[0].lastState.terminated.reason}' 2>/dev/null | sed 's/^$/-/'
}
wait_ready() { $K -n $NS wait --for=condition=ready "pod/$1" --timeout=300s >/dev/null 2>&1; }
gone() { $K -n $NS delete pod "$@" --wait=true --timeout=180s >/dev/null 2>&1; }

# ── M1 ────────────────────────────────────────────────────────────────
echo; echo "── M1: cold then warm, no cache vs the default-sized cache, on mid (512 MiB, fits) and big (6 GiB, six times the cache) (n=$REPS)"
row "M1	set	arm	rep	cold_ms	warm_ms	worker"
for set in mid big; do
    files=$FILES; [ "$set" = mid ] && files=$MID_FILES
    for arm in nocache cache; do
        for r in $(seq 1 "$REPS"); do
            p="m1-$set-$arm-$r"
            reader_pod "$p" "$set-$arm" | $K apply -f - >/dev/null
            wait_ready "$p" || { row "M1	$set	$arm	$r	NOTREADY	-	-"; gone "$p"; continue; }
            w=$(worker_of "$p")
            cold=$(read_all "$p" "$files"); warm=$(read_all "$p" "$files")
            row "M1	$set	$arm	$r	$cold	$warm	$w"
            gone "$p"
            # The worker's departure empties its emptyDir: the next rep is cold again.
            i=0; while [ $i -lt 60 ] && $K -n $WNS get pod "$w" >/dev/null 2>&1; do sleep 2; i=$((i + 2)); done
        done
    done
done

# ── M2 ────────────────────────────────────────────────────────────────
echo; echo "── M2: $READERS concurrent readers of 6 GiB through ONE shared mounter, three memory targets"
row "M2	arm	target_mib	limit	readers_ok	slowest_ms	worker_peak_mib	worker_terminated	worker"
m2_arm() { # <label> <cr> <target> <limit>
    local label=$1 cr=$2 target=$3 limit=$4 pods="" p w peak term okc=0 slow=0 ms
    for i in $(seq 1 "$READERS"); do p="m2-$label-$i"; pods="$pods $p"; reader_pod "$p" "$cr" "$NODE" | $K apply -f - >/dev/null; done
    for p in $pods; do wait_ready "$p" || echo "  $p not Ready: $(mount_events "$p" | tail -1 | cut -c1-160)"; done
    w=$(worker_of_any "${pods# }"); [ -n "$w" ] || w=$($K -n $WNS get pods -o json | python3 -c "
import json,sys
for p in json.load(sys.stdin)['items']:
    a=p['metadata'].get('annotations',{})
    if a.get('chert.us/cr')=='$cr' and 'chert.us/shared-mount' in a: print(p['metadata']['name']); break")
    echo "  shared worker: ${w:-?} (argv: $(worker_argv "$w" | grep -o -- '--memory-target [0-9]*'))"
    # all readers at once, each reading all six files 6-wide
    for p in $pods; do ( ms=$(read_all "$p"); echo "$p $ms" > "/tmp/m2-$p.out" ) & done; wait
    for p in $pods; do ms=$(awk '{print $2}' "/tmp/m2-$p.out"); rm -f "/tmp/m2-$p.out"; case "$ms" in FAIL|"") ;; *) okc=$((okc + 1)); [ "$ms" -gt "$slow" ] && slow=$ms;; esac; done
    peak=$(worker_peak_mib "$w"); term=$(worker_term "$w")
    row "M2	$label	$target	$limit	$okc/$READERS	$slow	${peak:--}	${term:--}	${w:-?}"
    gone $pods
    i=0; while [ $i -lt 90 ] && [ -n "$w" ] && $K -n $WNS get pod "$w" >/dev/null 2>&1; do sleep 2; i=$((i + 2)); done
}
m2_arm fix     big-shared     682  1Gi
m2_arm old     big-shared-old 973  1Gi
chart_up --set workers.sharedResources.limits.memory=4Gi --set workers.sharedResources.requests.memory=256Mi >/dev/null 2>&1 && plugin_rolled && echo "  chart: sharedResources 4Gi"
m2_arm big     big-shared     2730 4Gi
chart_up >/dev/null 2>&1 && plugin_rolled && echo "  chart restored"

delete_fx measure-tenants.yaml --ignore-not-found >/dev/null 2>&1
echo; echo "written: $OUT"
