#!/usr/bin/env bash
# The PASSTHROUGH door, with its cache actually configured.
#
# WHY THIS EXISTS. The 2026-09-10 door drill measured a mount-s3 with no
# `--cache` in its argv and wrote down that passthrough "caches
# NOTHING". That is true of THAT MOUNT, and it is not a property of the
# door: mount-s3 ships a content cache, and a deployment that cares
# about re-reads turns it on. Comparing lean's materialised tree against
# an unconfigured mount flatters lean on exactly the axis the
# comparison is about. This drill fixes that.
#
# THREE ARMS, ONE DIMENSION EACH. `--cache DIR` does TWO things: it
# caches object CONTENT and it moves metadata TTL from `minimal` to 60
# seconds. An A/B of "cache off" against "cache on" therefore cannot say
# which half produced the difference — and for a small-file tree the
# metadata half can dominate, because every open() is a HEAD when the
# TTL is minimal.
#
#   P        no cache, --metadata-ttl minimal   the 2026-09-10 baseline
#   P-meta   no cache, --metadata-ttl 60        the METADATA half alone
#   P-cache  --cache DIR (implies ttl 60)       both halves
#
# So P-meta - P isolates metadata caching, and P-cache - P-meta isolates
# CONTENT caching. Reporting only P vs P-cache would have attributed all
# of it to content, which is the mistake this layout exists to prevent.
#
# COLD AND WARM ARE DIFFERENT QUESTIONS and are never added up:
#   cold  first read of every byte, page cache dropped, cache dir wiped
#   warm  the identical read again, nothing dropped
# For P the two should be ~equal (nothing is kept). For P-cache the warm
# read is the whole point. A P-cache whose warm read does NOT improve is
# a finding, not a rounding error.
set -uo pipefail
cd "$(dirname "$0")"

: "${DRILL_BUCKET:?set DRILL_BUCKET}"
: "${DRILL_PREFIX:=ranged-drill}"
: "${AWS_REGION:=us-west-1}"
: "${MNT:=/mnt/nvme/pt}"
: "${CACHE:=/mnt/nvme/ms3cache}"
: "${PAR:=32}"
REPS="${1:-3}"
WORKLOADS="${WORKLOADS:-big small mixed}"
OUT="results/passthrough-cache-$(date -u +%Y%m%d-%H%M%S).tsv"
mkdir -p results "$MNT"

drop_caches() { sync; echo 3 > /proc/sys/vm/drop_caches; }

umount_all() { for w in $WORKLOADS; do mountpoint -q "$MNT/$w" && umount "$MNT/$w"; done; sleep 1; }

mount_arm() { # <arm> <workload>
  local arm="$1" w="$2" d="$MNT/$w"
  mkdir -p "$d"
  mountpoint -q "$d" && umount "$d"
  local args=(--read-only --prefix "$DRILL_PREFIX/$w/files/" --region "$AWS_REGION")
  case "$arm" in
    P)       args+=(--metadata-ttl minimal) ;;
    P-meta)  args+=(--metadata-ttl 60) ;;
    P-cache) rm -rf "$CACHE"; mkdir -p "$CACHE"
             args+=(--cache "$CACHE" --max-cache-size 60000) ;;
  esac
  mount-s3 "$DRILL_BUCKET" "$d" "${args[@]}" >/dev/null 2>&1 || return 1
  # A mount that came up EMPTY times as an instant win. The old drill
  # learned this the expensive way; verify before every timed window.
  mountpoint -q "$d" || return 1
  [ -n "$(find "$d" -maxdepth 1 -type f -print -quit 2>/dev/null)" ] || return 1
  return 0
}

# Build the listing OUTSIDE the timed window: walking 20k FUSE dentries
# is `find`, not a read.
listing() { # <workload> -> path to list
  local w="$1" l="/mnt/nvme/ptlist-$w"
  [ -s "$l" ] || find "$MNT/$w" -type f > "$l"
  echo "$l"
}

read_all() { # <workload> -> "elapsed_ms bytes"
  local w="$1" l; l=$(listing "$w")
  local t0 t1
  t0=$(date +%s%N)
  xargs -a "$l" -P "$PAR" -n 16 cat > /dev/null 2>/dev/null
  t1=$(date +%s%N)
  echo "$(( (t1 - t0) / 1000000 ))"
}

echo -e "rep\tarm\tworkload\tphase\tms" | tee "$OUT"
for rep in $(seq 1 "$REPS"); do
  for w in $WORKLOADS; do
    # Arms interleaved WITHIN the workload within the rep: batching by
    # arm hands every drift in S3 weather and spot-neighbour noise to
    # whichever arm held that stretch of wall clock.
    for arm in P P-meta P-cache; do
      rm -f "/mnt/nvme/ptlist-$w"
      if ! mount_arm "$arm" "$w"; then
        echo "MOUNT-FAIL $arm/$w — skipping (an empty mount would time as a win)" >&2
        continue
      fi
      drop_caches
      cold=$(read_all "$w")
      warm=$(read_all "$w")          # nothing dropped: this is the re-read
      printf '%s\t%s\t%s\t%s\t%s\n' "$rep" "$arm" "$w" cold "$cold" | tee -a "$OUT"
      printf '%s\t%s\t%s\t%s\t%s\n' "$rep" "$arm" "$w" warm "$warm" | tee -a "$OUT"
      umount "$MNT/$w" 2>/dev/null
    done
  done
done

echo
echo "=== passthrough cache drill — RANGE over $REPS reps (ms) ==="
printf '%-8s %-8s %-6s %9s %9s\n' workload arm phase min max
for w in $WORKLOADS; do
  for arm in P P-meta P-cache; do
    for ph in cold warm; do
      awk -v w="$w" -v a="$arm" -v p="$ph" -F'\t' '
        $3==w && $2==a && $4==p { if (min=="" || $5<min) min=$5; if ($5>max) max=$5 }
        END { if (min!="") printf "%-8s %-8s %-6s %9d %9d\n", w, a, p, min, max }' "$OUT"
    done
  done
done
echo
echo "P-meta minus P isolates METADATA caching; P-cache minus P-meta isolates"
echo "CONTENT caching. Quote ranges. Overlapping ranges are NO MEASURED"
echo "DIFFERENCE, which is a result."
