#!/usr/bin/env bash
# The DOOR drill: same bytes, same bucket, same node, four doors.
#
# ranged-checkout-drill.sh answered a question about ONE door — does
# lean's checkout go faster if a large object is fetched as parallel
# ranges. This one asks the question that motivated it: for a cold read
# of the same tree, how do the doors compare?
#
#   L-base    flint-lean checkout, shipped defaults
#   L-opt     flint-lean checkout, ranged GET on
#   P         flint-passthrough (mount-s3) read through the mount
#   S         raw S3, `aws s3 cp --recursive`
#
# L-base and L-opt come from ranged-checkout-drill.sh and are NOT re-run
# here; this script measures P and S so the four can be put side by side.
#
# WHAT MAKES THIS A FAIR FIGHT, AND WHERE IT ISN'T
#
# The doors do different things, and pretending otherwise is how a
# benchmark lies. Lean and raw S3 MATERIALISE the tree: bytes land on
# NVMe and every later read is local-disk speed. Passthrough does NOT:
# the mount is ready in milliseconds and every read pays S3, forever. A
# single "which is faster" number hides that, so this drill reports
# three separate things and never adds them up:
#
#   cold    first full read of every byte
#   warm    the same read again, nothing dropped
#   meta    `find -type f`, no bytes read at all
#
# CONCURRENCY IS A DIMENSION, NOT A DEFAULT. Lean fans out 32-wide
# because a bulk checkout can. A passthrough mount serves whatever the
# application asks for, so a single-threaded `cat` loop measures `cat`,
# not the mount. P therefore runs at BOTH 1 and 32, and S is pinned to
# 32 to match lean's fan-out — the CLI's default of 10 would make that
# arm lose on a dimension the drill holds fixed everywhere else.
#
# WHY THE READS COME FROM THE HOST PATH, NOT FROM INSIDE THE POD
#
# The mount is one FUSE connection served by one mount-s3 process. The
# pod's /mnt/<w> and the kubelet path below are the SAME mount through
# the SAME connection; a container namespace adds nothing to read
# throughput. Reading from the host removes a `kubectl exec` — tens of
# milliseconds of API server, TLS and container attach — from a window
# whose fastest legs are a couple of seconds. It also keeps a
# cluster-admin kubeconfig off the worker node.
#
# THE GUARDS
#
#   bytes   every arm must read the SAME total bytes for a workload. An
#           arm that "wins" having read less has not won, and a door
#           serving a truncated or empty tree reads as a blazing result
#           — the failure shape that is easiest to write down as a win.
#   count   file count must match the seeded tree, same reason.
#   cold    a positive control for the CACHE DROP ITSELF, on the
#           load-bearing path: it calls the same drop_caches the P legs
#           call, then reads a LOCAL NVMe tree cold and warm. If those
#           two do not differ sharply, drop_caches is not dropping and
#           every "cold" number here is a warm read wearing a hat.
#   par     P-32 must beat P-1 on `small`. If fan-out does not move this
#           rig, its P numbers are noise about a term it is not varying.
#
# n=3 reps, interleaved within a rep, and the report quotes RANGES.
set -euo pipefail

MODE="${1:-run}"
REPS="${2:-3}"
: "${DRILL_BUCKET:?set DRILL_BUCKET}"
: "${DRILL_PREFIX:=ranged-drill}"
: "${DRILL_ROOT:=/mnt/nvme/drill}"
: "${AWS_REGION:=us-west-1}"
export AWS_REGION AWS_DEFAULT_REGION="$AWS_REGION"
mkdir -p "$DRILL_ROOT/results"
RESULTS="$DRILL_ROOT/results/door-$(date -u +%Y%m%d-%H%M%S).tsv"

# The seeded truth, from the same generator the lean arms read.
declare -A WANT_FILES=( [big]=6           [small]=20000     [mixed]=2001       )
declare -A WANT_BYTES=( [big]=6442450944  [small]=163840000 [mixed]=4327735296 )
WORKLOADS="big small mixed"

# The kubelet bind path for one FlintPassthroughMount volume. Resolved
# fresh every call: the pod UID changes the moment the pod is recreated,
# and a stale path is an empty directory, which is a 0.00s "win".
mnt_path() { # <volume name>
  awk -v v="/$1/mount" '$1=="mount-s3" && index($2,"/pods/") && index($2,v){print $2}' /proc/mounts | head -1
}

drop_caches() { sync; echo 3 > /proc/sys/vm/drop_caches; }

# ── P: read every file through the mount, N ways wide ────────────────
#
# The listing is built ONCE, outside the timed window: walking 20k FUSE
# dentries 32 times over would measure `find`, not the reads.
pod_read() { # <workload> <par> -> "elapsed_ms bytes files"
  local w="$1" par="$2" m; m=$(mnt_path "$w")
  [ -n "$m" ] || { echo "0 0 0"; return; }
  local list="$DRILL_ROOT/list-$w"
  [ -s "$list" ] || find "$m" -type f > "$list"
  local n; n=$(wc -l < "$list")
  local t0 t1 i
  t0=$(date +%s%N)
  for ((i = 0; i < par; i++)); do
    awk -v p="$par" -v k="$i" 'NR % p == k' "$list" \
      | while IFS= read -r f; do cat -- "$f"; done > /dev/null &
  done
  wait
  t1=$(date +%s%N)
  # Sizes come from the same listing the reads walked, counted OUT of
  # the window: `cat | wc -c` would put a second process and a pipe in
  # the measured path.
  local b; b=$(xargs -a "$list" -d '\n' stat -c %s 2>/dev/null | awk '{s+=$1} END {print s+0}')
  echo "$(( (t1 - t0) / 1000000 )) $b $n"
}

pod_meta() { # <workload> -> "elapsed_ms files"
  local w="$1" m; m=$(mnt_path "$w")
  [ -n "$m" ] || { echo "0 0"; return; }
  local t0 t1 n
  t0=$(date +%s%N); n=$(find "$m" -type f | wc -l); t1=$(date +%s%N)
  echo "$(( (t1 - t0) / 1000000 )) $n"
}

# ── S: raw S3, the thing a person actually types ─────────────────────
s3_copy() { # <workload> -> "elapsed_ms bytes files"
  local w="$1" dir="$DRILL_ROOT/s3arm-$w"
  rm -rf "$dir"; mkdir -p "$dir"
  local t0 t1
  t0=$(date +%s%N)
  AWS_MAX_ATTEMPTS=5 aws s3 cp --recursive --quiet \
    "s3://$DRILL_BUCKET/$DRILL_PREFIX/$w/files" "$dir"
  t1=$(date +%s%N)
  # Sum FILE sizes, not `du -sb`: du counts the directory inode too, so
  # this arm reports 4096 bytes more than it read and trips the byte
  # guard for a reason that has nothing to do with S3.
  echo "$(( (t1 - t0) / 1000000 )) $(find "$dir" -type f -printf '%s\n' | awk '{s+=$1} END {print s+0}') $(find "$dir" -type f | wc -l)"
}

# ── the cache-drop control ───────────────────────────────────────────
cold_control() { # -> "cold_ms warm_ms"
  local dir="$DRILL_ROOT/s3arm-big"
  [ -d "$dir" ] && [ -n "$(ls -A "$dir" 2>/dev/null)" ] || { echo "0 0"; return; }
  local t0 t1 cold warm
  drop_caches
  t0=$(date +%s%N); cat "$dir"/* > /dev/null; t1=$(date +%s%N); cold=$(( (t1-t0)/1000000 ))
  t0=$(date +%s%N); cat "$dir"/* > /dev/null; t1=$(date +%s%N); warm=$(( (t1-t0)/1000000 ))
  echo "$cold $warm"
}

row() { printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$@" | tee -a "$RESULTS"; }

case "$MODE" in
  shakedown)
    for w in $WORKLOADS; do
      printf '%-6s mnt=%s meta=%s\n' "$w" "$(mnt_path "$w")" "$(pod_meta "$w")"
    done
    ;;
  run)
    echo "results -> $RESULTS" >&2
    for rep in $(seq 1 "$REPS"); do
      for w in $WORKLOADS; do
        for par in 1 32; do
          drop_caches
          read -r ms b n <<<"$(pod_read "$w" "$par")"
          row "$rep" "$w" "P-cold-$par" "$ms" "$b" "$n"
        done
        # warm: immediately after the cold-32 leg, nothing dropped.
        read -r ms b n <<<"$(pod_read "$w" 32)"
        row "$rep" "$w" "P-warm-32" "$ms" "$b" "$n"

        drop_caches
        read -r ms n <<<"$(pod_meta "$w")"
        row "$rep" "$w" "P-meta" "$ms" "-" "$n"

        drop_caches
        read -r ms b n <<<"$(s3_copy "$w")"
        row "$rep" "$w" "S-cli-32" "$ms" "$b" "$n"
      done
      read -r c wm <<<"$(cold_control)"
      row "$rep" "-" "ctl-dropcache" "$c" "$wm" "-"
    done

    # ── guards ───────────────────────────────────────────────────────
    fail=0
    for w in $WORKLOADS; do
      while read -r arm b n; do
        [ "$b" = "-" ] && continue
        if [ "$b" != "${WANT_BYTES[$w]}" ]; then
          echo "GUARD FAIL [$w/$arm]: read $b bytes, seeded ${WANT_BYTES[$w]} — an arm that reads fewer bytes has not won" >&2
          fail=1
        fi
        if [ "$n" != "${WANT_FILES[$w]}" ]; then
          echo "GUARD FAIL [$w/$arm]: saw $n files, seeded ${WANT_FILES[$w]} — this door is serving a different tree" >&2
          fail=1
        fi
      done < <(awk -v w="$w" '$2==w {print $3"\t"$5"\t"$6}' "$RESULTS")
    done

    read -r c wm <<<"$(awk '$3=="ctl-dropcache"{c+=$4; w+=$5; n++} END{if(n) printf "%d %d", c/n, w/n}' "$RESULTS")"
    if [ -n "${c:-}" ] && [ "$c" -le $(( ${wm:-0} * 2 )) ]; then
      echo "GUARD FAIL [control]: local cold ${c}ms vs warm ${wm}ms — drop_caches is not dropping, every 'cold' here is a warm read" >&2
      fail=1
    fi

    read -r p1 p32 <<<"$(awk '$2=="small" && $3=="P-cold-1"{a+=$4;x++} $2=="small" && $3=="P-cold-32"{b+=$4;y++} END{if(x&&y) printf "%d %d", a/x, b/y}' "$RESULTS")"
    if [ -n "${p1:-}" ] && [ "$p1" -le $(( ${p32:-0} * 2 )) ]; then
      echo "GUARD FAIL [control]: small P-1 ${p1}ms vs P-32 ${p32}ms — fan-out does not move this rig, so its P numbers are noise about a term it is not varying" >&2
      fail=1
    fi

    if [ "$fail" = 0 ]; then echo "GUARDS PASSED"; else
      echo "GUARDS FAILED — do not quote these numbers." >&2; exit 1; fi
    ;;
  *) echo "usage: $0 {run [reps]|shakedown}" >&2; exit 2 ;;
esac
