#!/usr/bin/env bash
# Two MEASUREMENTS on a real cluster, after run-s3csi.sh setup (the same
# rig, the same helpers, imported the way run-legs.sh does):
#
#   M1  the block cache's cost AS DEPLOYED: a read cold then warm through
#       a mount with no cache, through one with a 768 MiB cache named on
#       the CR on the worker's scratch emptyDir (the sharing default of
#       2026-09-29, withdrawn by §11 step 1 once this measured it), and —
#       with CACHE_HOST_PATH set — through the same CR with the cache
#       PLACED there by workers.cacheHostPath (§11 step 2); n reps each,
#       on TWO sets — `mid` (4 × 128 MiB, fits the cache) and `big`
#       (6 × 1 GiB, eight times the cache, so the warm read can only
#       miss). The emptyDir sits on the node's root disk (an 8 GiB gp3 at
#       125 MiB/s on the trove i4i.large nodes), which the 09-12 door
#       drill on host NVMe with a 60 GB cache never measured; the
#       placement is the instance store this script mounts first (below).
#       Beside the times: whether the tenant got the CacheOnRootDisk note
#       (§11 step 3) and, for the placed arm, how much the cold read left
#       in the cache directory on the device.
#   M2  a SHARED mounter under N concurrent readers of the same 6 GiB:
#       the worker's memory peak and whether it is OOM-killed, with no
#       cache at all (the control: the mounter and S3, no disk), then
#       with the 768 MiB cache on the emptyDir at the plugin's two-thirds
#       target (682 at 1Gi), at mount-s3's own 95% default (973 — every
#       mount before 2026-09-30), and at a 4Gi workers.sharedResources
#       limit (2730); with CACHE_HOST_PATH set, the two-thirds arm once
#       more with the cache placed.
#
#   KUBECONFIG=… CTX=s3a STORE=s3 NODE_EXEC=nodesh TAG=… BUCKET=… S3_KEY_FILE=… \
#       [CACHE_HOST_PATH=/mnt/nvme/flint-s3-cache] [RUN_M1=0] [M2_ARMS="nocache placed"] \
#       ./aws-measure.sh [reps] [readers]
#
# RUN_M1=0 skips M1; M2_ARMS names the M2 arms to run (default all five,
# in the order above) — for a control added after a run, without paying
# for the rest again.
#
# CACHE_HOST_PATH is the directory workers.cacheHostPath will name. Its
# parent is mounted first on EVERY node (the plugin mounts the root as a
# type-Directory hostPath on every node it runs on, so a node without it
# fails its plugin pod) from the node's instance store: the one disk
# whose model is "Amazon EC2 NVMe Instance Storage" — never by its /dev
# name, which is not stable across identical instances — formatted xfs
# if it is blank. Mounting the device is the platform's job (on trove
# nodes a trove change, not a chart one); here the drill stands in for
# the platform and prints what it did. Unset, the placed arms are skipped
# and the script measures what it did before.
#
# Numbers, not verdicts: every cell is printed and written to
# results/measure-<date>.tsv. STORE=minio runs too, but then the store
# is one pod on one node and the numbers say more about it than about
# the cache — the header says which.
set -u
cd "$(dirname "$0")"
REPS=${1:-3}
READERS=${2:-4}
CACHE_HOST_PATH=${CACHE_HOST_PATH:-}
RUN_M1=${RUN_M1:-1}
M2_ARMS=${M2_ARMS:-nocache fix old big placed}
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
row "# measure $(date -u +%FT%TZ) ctx=$CTX store=$STORE node=$NODE tag=$TAG reps=$REPS readers=$READERS cache_host_path=${CACHE_HOST_PATH:--}"

# ── the placement: the instance store, mounted on every node ──────────
# The plugin stands in for nobody here: the directory must exist on
# every node before workers.cacheHostPath is set, or the plugin pod on
# the node without it fails (by design — §11 step 2). The device is
# found by MODEL, not by /dev name (two identical i4i.xlarge nodes have
# enumerated nvme0n1/nvme1n1 in opposite orders).
place_cache_device() {
    local n out mnt; mnt=$(dirname "$CACHE_HOST_PATH")
    for n in $($K get nodes -o jsonpath='{.items[*].metadata.name}'); do
        out=$(NODE=$n onnode 'set -u; root='"$CACHE_HOST_PATH"'; mnt='"$mnt"'
dev=$(lsblk -dnpo NAME,MODEL 2>/dev/null | awk "/Instance Storage/{print \$1}")
n=$(printf "%s\n" "$dev" | grep -c .)
[ "$n" = 1 ] || { echo "REFUSING: $n instance-store disk(s) [$dev]"; exit 1; }
if ! mountpoint -q "$mnt"; then
    blkid -p "$dev" >/dev/null 2>&1 || mkfs.xfs -q "$dev" 2>&1 || { echo "mkfs.xfs $dev failed"; exit 1; }
    mkdir -p "$mnt" && mount "$dev" "$mnt" 2>&1 || { echo "mount $dev $mnt failed"; exit 1; }
fi
mkdir -p "$root" && chmod 755 "$root"
echo "$dev on $mnt: $(df -h --output=size,used,fstype "$mnt" | tail -1 | tr -s " ")"')
        echo "  $n: ${out:-no answer}"
        case "$out" in *" on $mnt: "*) ;; *) return 1 ;; esac
    done
}
if [ -n "$CACHE_HOST_PATH" ]; then
    echo "── placing the cache: $CACHE_HOST_PATH on the instance store of every node (the drill as the platform)"
    if ! place_cache_device; then
        echo "  the placed arms are SKIPPED: not every node has the device mounted" >&2
        row "# placed arms skipped: $CACHE_HOST_PATH could not be made on every node"
        CACHE_HOST_PATH=""
    fi
fi

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
# The CacheOnRootDisk notes on a tenant pod (0 or 1 — one per publish).
notes_on() { mount_events "$1" | grep -c '^CacheOnRootDisk' | tr -d ' '; }
# The plugin's last "block cache device" line: both devices, major:minor.
cache_device_line() { plugin_log | grep 'block cache device' | tail -1 | grep -o 'cache_device.*' | cut -c1-120; }
# The chart with the cache placed, and back. Each roll is every node's
# plugin pod; a node without the root fails here, loudly.
chart_placed() { chart_up --set workers.cacheHostPath="$CACHE_HOST_PATH" >/dev/null 2>&1 && plugin_rolled && echo "  chart: workers.cacheHostPath=$CACHE_HOST_PATH"; }
chart_plain()  { chart_up >/dev/null 2>&1 && plugin_rolled && echo "  chart restored"; }

# ── M1 ────────────────────────────────────────────────────────────────
arms="nocache cache"; [ -n "$CACHE_HOST_PATH" ] && arms="$arms placed"
[ "$RUN_M1" = 1 ] || arms=""
echo; echo "── M1: cold then warm, arms [${arms:-skipped}], on mid (512 MiB, fits the cache) and big (6 GiB, eight times it) (n=$REPS)"
row "M1	arm	set	rep	cold_ms	warm_ms	note	placed_mib	worker"
for arm in $arms; do
    if [ "$arm" = placed ]; then chart_placed || { row "M1	placed	-	-	CHART_FAILED	-	-	-	-"; continue; }; fi
    for set in mid big; do
        files=$FILES; [ "$set" = mid ] && files=$MID_FILES
        # The placed arm reads through the SAME CR as the emptyDir arm:
        # only the chart differs, which is the point.
        cr="$set-$arm"; [ "$arm" = placed ] && cr="$set-cache"
        for r in $(seq 1 "$REPS"); do
            p="m1-$set-$arm-$r"
            # Pinned to $NODE: the device, the directory and the plugin
            # log read below are that node's, and a second worker would
            # otherwise take some of the pods.
            reader_pod "$p" "$cr" "$NODE" | $K apply -f - >/dev/null
            wait_ready "$p" || { row "M1	$arm	$set	$r	NOTREADY	-	-	-	-"; echo "  $(mount_events "$p" | tail -1 | cut -c1-200)"; gone "$p"; continue; }
            w=$(worker_of "$p")
            cold=$(read_all "$p" "$files")
            pm=-; [ "$arm" = placed ] && pm=$(onnode "du -sm $CACHE_HOST_PATH/$w 2>/dev/null | cut -f1")
            warm=$(read_all "$p" "$files")
            row "M1	$arm	$set	$r	$cold	$warm	$(notes_on "$p")	${pm:--}	$w"
            gone "$p"
            # The worker's departure empties its cache directory: the next rep is cold again.
            i=0; while [ $i -lt 60 ] && $K -n $WNS get pod "$w" >/dev/null 2>&1; do sleep 2; i=$((i + 2)); done
        done
    done
    [ "$arm" != nocache ] && echo "  plugin on $NODE: $(cache_device_line)"
    if [ "$arm" = placed ]; then
        echo "  $CACHE_HOST_PATH on $NODE after the arm: [$(onnode "ls -A $CACHE_HOST_PATH | tr '\n' ' '")] (empty = every directory went with its worker)"
        chart_plain
    fi
done

# ── M2 ────────────────────────────────────────────────────────────────
echo; echo "── M2: $READERS concurrent readers of 6 GiB through ONE shared mounter — arms [$M2_ARMS]"
row "M2	arm	target_mib	limit	readers_ok	slowest_ms	worker_peak_mib	worker_terminated	note	worker"
m2_arm() { # <label> <cr> <target> <limit>
    local label=$1 cr=$2 target=$3 limit=$4 pods="" p w peak term okc=0 slow=0 ms nt=-
    for i in $(seq 1 "$READERS"); do p="m2-$label-$i"; pods="$pods $p"; reader_pod "$p" "$cr" "$NODE" | $K apply -f - >/dev/null; done
    for p in $pods; do wait_ready "$p" || echo "  $p not Ready: $(mount_events "$p" | tail -1 | cut -c1-160)"; done
    w=$(worker_of_any "${pods# }"); [ -n "$w" ] || w=$($K -n $WNS get pods -o json | python3 -c "
import json,sys
for p in json.load(sys.stdin)['items']:
    a=p['metadata'].get('annotations',{})
    if a.get('chert.us/cr')=='$cr' and 'chert.us/shared-mount' in a: print(p['metadata']['name']); break")
    echo "  shared worker: ${w:-?} (argv: $(worker_argv "$w" | grep -o -- '--memory-target [0-9]*' || echo 'no --cache'))"
    # all readers at once, each reading all six files 6-wide
    for p in $pods; do ( ms=$(read_all "$p"); echo "$p $ms" > "/tmp/m2-$p.out" ) & done; wait
    # The note is counted AFTER the reads: the recorder lags the publish
    # by seconds, and a count taken at Ready read 0 on four pods whose
    # events were there a moment later (s3a, 2026-10-01).
    nt=$(notes_on "${pods# }")
    for p in $pods; do ms=$(awk '{print $2}' "/tmp/m2-$p.out"); rm -f "/tmp/m2-$p.out"; case "$ms" in FAIL|"") ;; *) okc=$((okc + 1)); [ "$ms" -gt "$slow" ] && slow=$ms;; esac; done
    peak=$(worker_peak_mib "$w"); term=$(worker_term "$w")
    row "M2	$label	$target	$limit	$okc/$READERS	$slow	${peak:--}	${term:--}	${nt:--}	${w:-?}"
    gone $pods
    i=0; while [ $i -lt 90 ] && [ -n "$w" ] && $K -n $WNS get pod "$w" >/dev/null 2>&1; do sleep 2; i=$((i + 2)); done
}
for a in $M2_ARMS; do
    case "$a" in
        nocache) m2_arm nocache big-shared-nocache 682 1Gi ;;
        fix)     m2_arm fix     big-shared         682 1Gi ;;
        old)     m2_arm old     big-shared-old     973 1Gi ;;
        big)
            chart_up --set workers.sharedResources.limits.memory=4Gi --set workers.sharedResources.requests.memory=256Mi >/dev/null 2>&1 && plugin_rolled && echo "  chart: sharedResources 4Gi"
            m2_arm big big-shared 2730 4Gi
            chart_plain ;;
        placed)
            [ -n "$CACHE_HOST_PATH" ] || { echo "  M2 placed: skipped (no CACHE_HOST_PATH)"; continue; }
            chart_placed || continue
            m2_arm placed big-shared 682 1Gi
            echo "  plugin on $NODE: $(cache_device_line)"
            chart_plain ;;
        *) echo "  M2: no arm '$a'" >&2 ;;
    esac
done

delete_fx measure-tenants.yaml --ignore-not-found >/dev/null 2>&1
echo; echo "written: $OUT"
