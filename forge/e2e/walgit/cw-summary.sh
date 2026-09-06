#!/usr/bin/env bash
# S3 request metrics per arm for the windows run-compare.sh recorded.
#
#   BUCKET=... ./forge/e2e/walgit/cw-summary.sh <work-dir>
#
# The bucket carries two metrics configurations, FilterId `forge` and
# `walgit`, one per prefix (created with the bucket). CloudWatch's S3
# request metrics arrive with a delay of several minutes and are summed
# per minute, so a window is widened to whole minutes and the numbers
# are what S3 counted, not what git sent. Run it twenty minutes after
# the campaign; if a row says "no data yet", run it again later.
set -uo pipefail
: "${BUCKET:?BUCKET}"
W=${1:?work dir with windows.txt}
REGION=${REGION:-us-west-1}
[ -f "$W/windows.txt" ] || { echo "no $W/windows.txt"; exit 2; }
iso() { date -u -r "$1" +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || date -u -d "@$1" +%Y-%m-%dT%H:%M:%SZ; }
sum() { # filter metric start end
  aws cloudwatch get-metric-statistics --region "$REGION" --namespace AWS/S3 --metric-name "$2" \
      --dimensions Name=BucketName,Value="$BUCKET" Name=FilterId,Value="$1" \
      --start-time "$(iso "$3")" --end-time "$(iso "$4")" --period 60 --statistics Sum \
      --query 'sum(Datapoints[].Sum)' --output text 2>/dev/null
}
printf '%-4s %-7s %-8s %12s %12s %12s %12s %14s %14s\n' leg arm window AllRequests PutRequests GetRequests HeadRequests BytesUploaded BytesDownloaded
while read -r leg arm t0 t1; do
  s=$(( t0 / 60 * 60 )); e=$(( (t1 / 60 + 1) * 60 ))
  all=$(sum "$arm" AllRequests "$s" "$e"); put=$(sum "$arm" PutRequests "$s" "$e"); get=$(sum "$arm" GetRequests "$s" "$e"); head=$(sum "$arm" HeadRequests "$s" "$e"); up=$(sum "$arm" BytesUploaded "$s" "$e"); down=$(sum "$arm" BytesDownloaded "$s" "$e")
  case "$all" in None|"") printf '%-4s %-7s %-8s %s\n' "$leg" "$arm" "$((t1-t0))s" "no data yet"; continue;; esac
  printf '%-4s %-7s %-8s %12.0f %12.0f %12.0f %12.0f %14.0f %14.0f\n' "$leg" "$arm" "$((t1-t0))s" "$all" "${put:-0}" "${get:-0}" "${head:-0}" "${up:-0}" "${down:-0}"
done < "$W/windows.txt"
echo
echo "per-push figures: divide a P9 row by the pushes that leg made (P9_N, default 48) and a P2 row by the pushes it acknowledged (in the compare log)."
