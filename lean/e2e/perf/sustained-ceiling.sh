#!/usr/bin/env bash
# Is the measured read throughput a RATE, or is it BURST CREDIT?
#
# WHY THIS RUNS BEFORE ANY RATIO IS WRITTEN DOWN. On i4i.large the EC2
# API reports BaselineBandwidthInGbps 0.781 (= 93 MiB/s) against a
# PeakBandwidthInGbps of 10. Those are different numbers and only the
# first is a guarantee; the second is credit you can spend until it runs
# out. A 19-second read never leaves the credit window, so it measures
# how fast the burst is, not how fast the link is.
#
# This project has been here before. Three scaling claims from the
# 2026-08-02 pNFS session had to be RETRACTED once every headline figure
# turned out to sit at 105-140% of some node's guaranteed baseline, and
# the "clean linearity" from width 1 to 2 was five rate limiters summing.
# The drill's current `big` numbers are worse: 323 MiB/s ranged is 348%
# of this node's guarantee, and the 2026-09-10 passthrough figure works
# out to ~574%.
#
# THE INSTRUMENT IS /proc/net/dev, NOT THE TOOL'S OWN CLOCK. Bytes on
# the wire are counted by the kernel regardless of what the client
# thinks it did — no double-counting of retries, no crediting a cache
# hit as a transfer, and it cannot be fooled by a tool that reports
# success while reading nothing.
#
# WHAT EACH OUTCOME MEANS, decided before the run so the result cannot
# be read to taste:
#
#   decays toward ~93 MiB/s   every large-object figure in both tables
#                             is a burst artifact and must be LABELLED,
#                             and a bigger NIC would only buy a bigger
#                             burst.
#   holds well above 93       the guarantee is not the binding limit
#                             here, and the scaling question is worth a
#                             guaranteed-bandwidth node (baseline ==
#                             peak, e.g. i3en.6xlarge) to answer.
#   never reaches 93 at all   the CLIENT is the limit, not the network,
#                             and no NIC purchase changes anything.
set -uo pipefail
: "${DRILL_BUCKET:?set DRILL_BUCKET}"
: "${DRILL_PREFIX:=ranged-drill}"
: "${AWS_REGION:=us-west-1}"
: "${FLINT_SYNC_BIN:=/mnt/nvme/rig/flint-sync}"
MINUTES="${1:-8}"
# The GUARANTEE for this node, read from the EC2 API, never guessed:
#   aws ec2 describe-instance-types --instance-types <t> \
#     --query 'InstanceTypes[].NetworkInfo.NetworkCards[0].[BaselineBandwidthInGbps,PeakBandwidthInGbps]'
# 1 Gbps = 119.2 MiB/s.  i4i.large 0.781 -> 93.  i3en.6xlarge 25.0 -> 2980,
# and there baseline == peak, so there is NO credit to exhaust and any
# figure it produces is a RATE by construction.
GUARANTEE_MIB="${GUARANTEE_MIB:-93}"

IFACE=$(ip route show default | awk '{print $5; exit}')
rx() { awk -v i="$IFACE:" '$1==i {print $2}' /proc/net/dev; }

echo "iface=$IFACE  guarantee=${GUARANTEE_MIB} MiB/s  duration=${MINUTES}m"
echo "sampling /proc/net/dev every 10s"

# A continuous ranged checkout loop: lean's REAL read path under
# sustained load, not a synthetic GET. NVMe on i4i sustains GB/s, well
# above any figure here, so the disk cannot be the limit.
( while :; do
    d=/mnt/nvme/ceil-run; rm -rf "$d"; mkdir -p "$d"
    FLINT_SYNC_ROOT="$d" FLINT_SYNC_BUCKET="$DRILL_BUCKET" \
    FLINT_SYNC_PREFIX="$DRILL_PREFIX/big" FLINT_SYNC_FANOUT=32 \
    FLINT_SYNC_FETCH_INFLIGHT_MB=512 FLINT_SYNC_RANGE_GET_MIN_MB=8 \
    FLINT_SYNC_RANGE_GET_CHUNK_MB=16 FLINT_SYNC_RANGE_GET_PARALLELISM=4 \
    "$FLINT_SYNC_BIN" checkout >/dev/null 2>&1
  done ) &
LOOP=$!
trap 'kill $LOOP 2>/dev/null; pkill -f "flint-sync checkout" 2>/dev/null' EXIT

printf '%6s %10s %10s\n' elapsed MiB/s pct_of_guarantee
prev=$(rx)
for t in $(seq 1 $((MINUTES * 6))); do
  sleep 10
  cur=$(rx)
  mib=$(( (cur - prev) / 10 / 1048576 ))
  prev=$cur
  printf '%5ss %10s %9s%%\n' "$((t * 10))" "$mib" "$(( mib * 100 / GUARANTEE_MIB ))"
done
