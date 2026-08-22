#!/usr/bin/env bash
# D8 — the write reserve must not be defeated by write speed.
#
# The original drill leg put 600 MiB onto 539 MiB free and got 201 on
# every chunk: `df` to 0, the whole reserve gone, nospcWriteRefusals
# still 0. `admit_bytes` compared a 2s-stale gauge against ONE write's
# length, and ioops.rs calls it PER WRITE OP — so a streaming PUT
# outran the refresher.
#
# This leg PUTs more than the volume's headroom and asserts the hub
# REFUSES (507) rather than eating the reserve.
set -uo pipefail

NS="${NS:-d8}"
SHARE="${SHARE:-fs-d8}"
BUCKET="${BUCKET:?needs BUCKET}"
REGION="${REGION:-us-west-1}"
HUB_AK="${DRILL_AK:?needs DRILL_AK}"
HUB_SK="${DRILL_SK:?needs DRILL_SK}"
PVC_SIZE="${PVC_SIZE:-1Gi}"
PUT_MB="${PUT_MB:-900}"

PASSES=0; FAILURES=()
say()  { echo; echo "── $*"; }
pass() { echo "  ✓ $*"; PASSES=$((PASSES+1)); }
fail() { echo "  ✗ FAIL: $*"; exit 1; }
bad()  { echo "  ✗ FAIL: $*"; FAILURES+=("$*"); }
note() { echo "    · $*" >&2; }

cleanup() {
  kubectl delete ns "$NS" --wait=false >/dev/null 2>&1
  echo "  (namespace $NS deleting; bucket prefix d8/ left in place)"
}
trap cleanup EXIT


# The hub's export root inside its container (render.rs: DATA_MOUNT/exports).
EXPORT_ROOT=/data/exports
hub_pod() { kubectl -n "$NS" get pods -l flint.io/share=$SHARE \
  -o jsonpath='{.items[0].metadata.name}' 2>/dev/null; }
avail_bytes() {
  local p; p=$(hub_pod); [ -n "$p" ] || { echo ""; return; }
  kubectl -n "$NS" exec "$p" -- df -B1 "$EXPORT_ROOT" 2>/dev/null \
    | awk 'NR==2 {print $4}' | tr -dc '0-9'
}

echo "══════════════════════════════════════════════════════════════════"
echo " D8 — the write reserve vs a fast writer"
echo "══════════════════════════════════════════════════════════════════"

# ⚠ ANTI-VACUITY GATE — READ THIS BEFORE RUNNING.
#
# This leg is only meaningful on a storage class that ENFORCES the PVC
# size. The hub's guard reads statvfs of the export root; with
# `local-path` (a plain directory on the node root) statvfs reports the
# NODE's filesystem, so a "1Gi" PVC actually has the node's free space
# behind it. On runbu that was 7.9G total / 2.6G free, so tripping the
# guard would have meant filling a node root — dangerous, and it would
# have proven nothing about a 1Gi volume.
#
# Refuse rather than produce a green tick that means nothing.
SC="${STORAGE_CLASS:-}"
if [ -z "$SC" ]; then
  # The default class, read from the marker column rather than a
  # jsonpath with escaped dots — the escaping is the exact trap that
  # makes a CRD printer-column or CEL string blow up elsewhere in this
  # repo, and here it silently returned EMPTY (which only failed safe
  # by luck; a garbage value would have RUN the leg).
  SC=$(kubectl get sc --no-headers 2>/dev/null | awk '$2 ~ /default/ {print $1; exit}')
fi
if [ -z "$SC" ]; then
  SC=$(kubectl get sc --no-headers 2>/dev/null | awk 'NR==1 {print $1}')
fi
case "$SC" in
  local-path|""|hostpath|manual)
    echo "REFUSING TO RUN: storage class '${SC:-<none>}' does not enforce PVC size."
    echo "  The hub reads statvfs of the export root, so the volume would have the"
    echo "  whole NODE disk behind it and this leg would test nothing. Re-run with"
    echo "  STORAGE_CLASS=<a size-enforcing class> (e.g. a flint-csi block class)."
    exit 2 ;;
esac
echo "  storage class: $SC (size-enforcing — required for this leg to mean anything)"

kubectl create ns "$NS" >/dev/null 2>&1
kubectl -n "$NS" create secret generic flint-s3 \
  --from-literal=AWS_ACCESS_KEY_ID="$HUB_AK" \
  --from-literal=AWS_SECRET_ACCESS_KEY="$HUB_SK" \
  --dry-run=client -o yaml 2>/dev/null | kubectl apply -f - >/dev/null 2>&1
kubectl -n "$NS" create secret generic tok-d8 \
  --from-literal=token=d8-secret-token \
  --dry-run=client -o yaml 2>/dev/null | kubectl apply -f - >/dev/null 2>&1

say "a share with a deliberately small disk (${PVC_SIZE})"
kubectl apply -f - >/dev/null 2>&1 <<EOF || fail "share refused"
apiVersion: flint.io/v1alpha1
kind: FlintShare
metadata: { name: $SHARE, namespace: $NS, labels: { flint.io/project-id: d8 } }
spec:
  bucket: $BUCKET
  region: $REGION
  keyPrefix: d8/
  credentialsSecretRef: flint-s3
  persistence: { size: $PVC_SIZE, storageClassName: $SC }
  monitoring:
    enabled: true
    fileApi: { enabled: true, tokenSecretRef: tok-d8 }
EOF

for _ in $(seq 1 60); do
  P=$(kubectl -n "$NS" get flintshare "$SHARE" -o jsonpath='{.status.phase}' 2>/dev/null)
  [ "$P" = "Ready" ] && break
  sleep 5
done
[ "$P" = "Ready" ] || fail "share never Ready (phase=$P)"
pass "share Ready on a ${PVC_SIZE} disk"

API=$(kubectl -n "$NS" get flintshare "$SHARE" -o jsonpath='{.status.apiEndpoint}' 2>/dev/null)
[ -n "$API" ] || fail "no apiEndpoint published"
note "api: $API"

# ⚠ THE ORACLE IS RESERVE SURVIVAL, NOT THE HTTP CODE.
#
# Run 1 of this leg asserted "507 = fixed". It was VACUOUS: the UNFIXED
# 1.35.0 hub returned 507 too. The unfixed build also refuses eventually
# — it just refuses AFTER eating the reserve. The original D8 record says
# so exactly: "returned 201 and drove df to 0, whole reserve gone".
# So measure the RESERVE.
AVAIL_BEFORE=$(avail_bytes)
[ -n "$AVAIL_BEFORE" ] || fail "could not read df on $EXPORT_ROOT — no oracle, refusing to report"
note "avail before: $AVAIL_BEFORE bytes ($((AVAIL_BEFORE/1048576)) MiB)"
# Overshoot the true free space, whatever the filesystem overhead is.
PUT_MB=$(( AVAIL_BEFORE/1048576 + 150 ))
note "PUT sized to overshoot: ${PUT_MB} MiB"

say "PUT ${PUT_MB} MiB onto a disk with less headroom than that"
# In-cluster so the stream is at full speed — the whole point is to
# outrun the 2s gauge refresher, which a slow port-forward would not.
RAW=$(kubectl -n "$NS" run d8-put --rm -i --restart=Never --image=curlimages/curl:8.10.1 --command -- \
  sh -c "dd if=/dev/zero of=/tmp/big.bin bs=1M count=$PUT_MB 2>/dev/null; \
         curl -s -o /dev/null -w 'FLINTCODE=%{http_code}\n' -X PUT \
           -H 'Authorization: Bearer d8-secret-token' \
           -H 'Content-Type: application/octet-stream' -H 'Expect:' \
           -T /tmp/big.bin '$API/files/content?path=/big.bin'" 2>/dev/null)
# A SENTINEL, not a digit scrape: `kubectl run --rm` also prints
# `pod "d8-put" deleted`, and tr -dc '0-9' pulled the 8 out of the POD
# NAME — turning a perfectly good 507 into "5078" and failing the leg.
CODE=$(printf '%s' "$RAW" | sed -n 's/.*FLINTCODE=\([0-9][0-9]*\).*/\1/p' | tail -1)
[ -n "$CODE" ] || { note "raw: $(printf '%s' "$RAW" | tr '\n' ' ' | head -c 200)"; CODE="none"; }

echo "    · HTTP $CODE"
case "$CODE" in
  507)
    pass "REFUSED with 507 Insufficient Storage — the reserve held" ;;
  200|201|204)
    bad "the hub ACCEPTED ${PUT_MB} MiB it had no room for (HTTP $CODE) — this is D8" ;;
  *)
    bad "unexpected HTTP $CODE (wanted 507; 200/201 would be D8 reproducing)" ;;
esac

say "did the RESERVE survive?"
AVAIL_AFTER=$(avail_bytes)
[ -n "$AVAIL_AFTER" ] || bad "could not read df after the PUT — leg inconclusive"
if [ -n "$AVAIL_AFTER" ]; then
  note "avail after: $AVAIL_AFTER bytes ($((AVAIL_AFTER/1048576)) MiB)"
  # The reserve is 256 MiB (pnfs/config.rs:314). It is meant to be
  # INVIOLABLE — that is the whole point of a reserve.
  #
  # MEASURED BASELINE, unfixed flint-pnfs:1.35.0 on runbu 2026-08-22:
  # 841 MiB avail, 991 MiB PUT ⇒ **158 MiB left**. It answered 507, but
  # only after spending 98 MiB OF THE RESERVE. A floor of 64 MiB called
  # that a pass — twice — which is why this leg now measures against the
  # reserve itself and not an arbitrary floor.
  RESERVE=$((256*1048576))
  FLOOR=$(( RESERVE * 9 / 10 ))     # ~230 MiB: reserve essentially intact
  if [ "$AVAIL_AFTER" -ge "$FLOOR" ]; then
    pass "the reserve HELD: $((AVAIL_AFTER/1048576)) MiB free ≥ $((FLOOR/1048576)) MiB — the guard refused BEFORE spending it"
  else
    bad "the reserve was BREACHED by $(( (RESERVE-AVAIL_AFTER)/1048576 )) MiB: only $((AVAIL_AFTER/1048576)) MiB left of a $((RESERVE/1048576)) MiB reserve — this is D8"
  fi
fi

echo
echo "══════════════════════════════════════════════════════════════════"
echo " D8 summary — $PASSES checks passed"
echo "══════════════════════════════════════════════════════════════════"
echo " PVC size                  : $PVC_SIZE"
echo " attempted PUT             : ${PUT_MB} MiB"
echo " HTTP answer               : $CODE   (507 = reserve held; 201 = D8)"
echo " avail before / after      : $((AVAIL_BEFORE/1048576)) MiB / $((${AVAIL_AFTER:-0}/1048576)) MiB   (< 230 MiB after = reserve breached = D8)"
echo
if [ ${#FAILURES[@]} -eq 0 ]; then echo "ALL LEGS PASSED."; else
  echo "${#FAILURES[@]} LEG(S) FAILED:"; for f in "${FAILURES[@]}"; do echo "  ✗ $f"; done; exit 1
fi
