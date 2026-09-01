#!/bin/bash
# Field verification for the LAYOUTGET stripe-width fix (7fd7917b).
#
#   KC=<kubeconfig> CLUSTER=runby ./stripe-width-verify.sh red|green
#
# RED  = stock flint-pnfs:1.43.0        -> corruption MUST reproduce
# GREEN= flint-pnfs:stripe-width-...    -> corruption MUST be gone
#
# RED is not optional. runbx's lesson is that a GREEN leg alone proves
# nothing: the workload has to be shown capable of producing the failure
# on this cluster, today, before its absence means anything.
#
# Three conditions make the result meaningful, each bought with a wasted
# run somewhere in this campaign:
#
#  1. SETTLE. runbx's first GREEN was void because the push started ~30 s
#     after the image swap while DSes were still re-registering — two
#     variables. We block on 3/3 active before touching the registry.
#  2. PROVE THE BINARY. A fixed MDS prints `Number of DSes in stripe: 3`
#     for EVERY grant including the bounded `length=4096` reads; 1.43.0
#     prints 1 for those. That is a positive marker on the defect's own
#     path, not an absence.
#  3. PROVE THE PATH WAS TAKEN. stripe-width-gate.py exits 2 when a log
#     holds no bounded grants. Exit 2 is NOT a pass — it means the
#     workload never asked the question.
set -eu
KC=${KC:?kubeconfig path}
LEG=${1:?red|green}
CLUSTER=${CLUSTER:-runby}
GREEN_IMAGE=${GREEN_IMAGE:-dilipdalton/flint-pnfs:stripe-width-7fd7917b}
RED_IMAGE=${RED_IMAGE:-dilipdalton/flint-pnfs:1.43.0}
NS=flint-system
K="kubectl --kubeconfig $KC"
HERE=$(cd "$(dirname "$0")" && pwd)
OUT=${OUT:-/tmp/swv-$LEG}
mkdir -p "$OUT"

case "$LEG" in
  red)   IMAGE=$RED_IMAGE ;;
  green) IMAGE=$GREEN_IMAGE ;;
  *) echo "leg must be red|green"; exit 2 ;;
esac

echo "== leg=$LEG image=$IMAGE =="

# ── 1. put the MDS on the leg's image, at DEBUG ───────────────────────
# The gate reads encode lines that exist only at debug. At INFO it
# reports blindness rather than passing, but we would have burned the
# run — so set the level with the image, in one roll.
$K -n $NS set image deploy/flint-pnfs-mds mds="$IMAGE"
for i in 0 1 2; do
  $K -n $NS set image pod/flint-pnfs-ds-$i ds="$IMAGE" 2>/dev/null || \
    $K -n $NS set image sts/flint-pnfs-ds ds="$IMAGE" 2>/dev/null || true
done
$K -n $NS set env deploy/flint-pnfs-mds RUST_LOG=debug
$K -n $NS rollout status deploy/flint-pnfs-mds --timeout=5m

# ── 2. SETTLE: every DS active before any I/O ─────────────────────────
echo "== settling (3/3 DSes active, no recent rejections) =="
for i in $(seq 1 60); do
  active=$($K -n $NS logs deploy/flint-pnfs-mds --tail=400 2>/dev/null \
           | grep -oE "([0-9]+) active" | tail -1 | grep -oE "^[0-9]+" || echo 0)
  ready=$($K -n $NS get pods -l app=flint-pnfs-ds --no-headers 2>/dev/null \
          | grep -c "Running" || echo 0)
  echo "  settle $i: ds pods running=$ready mds-reported-active=$active"
  [ "$ready" -ge 3 ] && sleep 20 && break
  sleep 10
done

# Record the provenance marker BEFORE the workload, so a failure to swap
# is visible as a setup fault rather than mistaken for a result.
$K -n $NS get deploy/flint-pnfs-mds \
   -o jsonpath='{.spec.template.spec.containers[0].image}{"\n"}' | tee "$OUT/image.txt"
$K -n $NS get pods -l app=flint-pnfs-mds \
   -o jsonpath='{.items[0].status.containerStatuses[0].imageID}{"\n"}' | tee -a "$OUT/image.txt"

# ── 3. drive the registry workload that broke runbw/runbx ─────────────
echo "== driving the registry push (the runbx repro) =="
"$HERE/push-and-client.sh" 2>&1 | tee "$OUT/push.log" || echo "(push reported failure — expected on RED)"

# ── 4. collect and judge ─────────────────────────────────────────────
echo "== collecting =="
$K -n $NS logs deploy/flint-pnfs-mds --since=30m 2>/dev/null | gzip > "$OUT/mds-debug.log.gz"
$K -n $NS logs deploy/registry-flint --since=30m 2>/dev/null > "$OUT/registry.log" || true
fives=$(grep -c ' 500 ' "$OUT/registry.log" 2>/dev/null || echo 0)
nuls=$(grep -c 'parsing time' "$OUT/registry.log" 2>/dev/null || echo 0)

echo
echo "=== registry 500s: $fives ; 'parsing time' NUL errors: $nuls"
echo "=== stripe-width gate ==="
set +e
python3 "$HERE/stripe-width-gate.py" "$OUT/mds-debug.log.gz" --expect-width=3
gate=$?
set -e

echo
echo "──────── VERDICT ($LEG) ────────"
case "$LEG:$gate" in
  red:1)   echo "RED as required: gate FAILED (width divergence present). Control is live." ;;
  red:*)   echo "RED INCONCLUSIVE (gate=$gate): the workload did not reproduce the defect on"
           echo "  stock 1.43.0, so a GREEN leg would prove nothing. Do not proceed." ;;
  green:0) echo "GREEN PASS: every grant at the pinned width, bounded grants present."
           [ "$nuls" -eq 0 ] || echo "  ⚠ but registry still logged $nuls NUL errors — investigate." ;;
  green:2) echo "GREEN INCONCLUSIVE: no bounded grants in the log. NOT a pass — the workload"
           echo "  never exercised the path the defect lives on." ;;
  green:1) echo "GREEN FAIL: width divergence still present. The fix did not close it here." ;;
esac
echo "evidence: $OUT"
exit $gate
