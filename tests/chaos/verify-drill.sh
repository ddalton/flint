#!/usr/bin/env bash
# Post-drill verification checklist. Run after EVERY drill; appends one CSV
# row to results.csv and prints PASS/FAIL.
#
#   PRE_NODE=<node> PRE_UID=<uid> PRE_RESTARTS=<n> \
#   [READY_TIMEOUT=600] [EXPECT_RESCHEDULE=same|cross|any|none] [QUICK=1] \
#   [NOTES="..."] ./verify-drill.sh <phase> <drill> <t0-epoch>
#
# Checks: pod Ready + attribution, DB verdict, VA consistency, stale NVMe
# sessions, orphaned mounts, driver log scan, timing capture.
set -uo pipefail
cd "$(dirname "$0")"
. ./lib.sh

PHASE=${1:?phase}; DRILL=${2:?drill}; T0=${3:?t0-epoch}
READY_TIMEOUT=${READY_TIMEOUT:-600}
EXPECT_RESCHEDULE=${EXPECT_RESCHEDULE:-any}
NOTES=${NOTES:-}
need_env

ART="$ARTIFACTS/${PHASE}-${DRILL}-${T0}"
mkdir -p "$ART"
FAILED=""

# 1. pod Ready + attribution -------------------------------------------------
step "1/7 pod Ready (timeout ${READY_TIMEOUT}s)"
if kubectl wait --for=condition=Ready pod/$PG -n "$NS" --timeout="${READY_TIMEOUT}s" >/dev/null 2>&1; then
  T_READY=$(( $(epoch) - T0 ))
  ok "pg-0 Ready ${T_READY}s after T0"
else
  T_READY=-1; FAILED="$FAILED ready"
  note "pg-0 NOT Ready within ${READY_TIMEOUT}s"
fi
POST_NODE=$(pg_node); POST_UID=$(pg_uid); POST_RESTARTS=$(pg_restarts)
RESTART_DELTA=$(( ${POST_RESTARTS:-0} - ${PRE_RESTARTS:-0} ))
if [ -n "${PRE_UID:-}" ]; then
  if [ "$POST_UID" = "${PRE_UID}" ]; then KIND="in-place"; else KIND="rescheduled"; fi
  case "$EXPECT_RESCHEDULE" in
    none) [ "$KIND" = "in-place" ] || { FAILED="$FAILED attribution"; note "expected in-place, got $KIND"; } ;;
    same) { [ "$KIND" = "rescheduled" ] && [ "$POST_NODE" = "${PRE_NODE:-}" ]; } || { FAILED="$FAILED attribution"; note "expected same-node replace"; } ;;
    cross) { [ "$KIND" = "rescheduled" ] && [ "$POST_NODE" != "${PRE_NODE:-}" ]; } || { FAILED="$FAILED attribution"; note "expected cross-node replace"; } ;;
  esac
  ok "attribution: $KIND (${PRE_NODE:-?}→${POST_NODE:-?}, postgres restarts +${RESTART_DELTA})"
fi

# 2. DB verdict ---------------------------------------------------------------
step "2/7 db verdict"
if QUICK=${QUICK:-0} ./verify-db.sh "$T0" > "$ART/db-verdict.txt" 2>&1; then
  DB=PASS; ok "db PASS"
else
  DB=FAIL; FAILED="$FAILED db"; note "db FAIL — see $ART/db-verdict.txt"
fi

# 3. VolumeAttachment consistency ---------------------------------------------
step "3/7 volumeattachments"
VA_OK=Y
PV=$(pg_pv)
kubectl get volumeattachments -o json > "$ART/vas.json" 2>/dev/null
if [ -n "$PV" ] && [ "$T_READY" -ge 0 ]; then
  VA_NODE=$(va_node_for_pv "$PV")
  N_VA=$(va_for_pv "$PV" | grep -c . || true)
  { [ "$N_VA" = "1" ] && [ "$VA_NODE" = "$POST_NODE" ]; } \
    || { VA_OK=N; FAILED="$FAILED va"; note "VA wrong: count=$N_VA node=${VA_NODE:-none} (pod on $POST_NODE)"; }
fi
STALE=$(stale_vas 120)
[ -z "$STALE" ] || { VA_OK=N; FAILED="$FAILED stale-va"; note "stale VAs: $STALE"; }
[ "$VA_OK" = "Y" ] && ok "VA consistent (1 VA on $POST_NODE, none stale)"

# 4. NVMe sessions ------------------------------------------------------------
step "4/7 nvme sessions"
NVME_OK=Y
for n in $(worker_nodes); do
  OUT=$(nvme_subsys "$n")
  echo "== $n ==" >> "$ART/nvme.txt"; echo "$OUT" >> "$ART/nvme.txt"
  if echo "$OUT" | grep -q "connecting"; then
    NVME_OK=N; FAILED="$FAILED nvme"; note "$n: controller stuck connecting"
  fi
done
[ "$NVME_OK" = "Y" ] && ok "no controllers stuck connecting (state in $ART/nvme.txt)"

# 5. mounts -------------------------------------------------------------------
step "5/7 orphaned mounts"
MOUNTS_OK=Y
for n in $(worker_nodes); do
  GM=$(globalmounts "$n")
  ORPH=$(orphan_pod_mounts "$n")
  echo "$n globalmounts=$GM orphans='${ORPH}'" >> "$ART/mounts.txt"
  [ -z "$ORPH" ] || { MOUNTS_OK=N; FAILED="$FAILED mounts"; note "$n orphan pod mounts: $ORPH"; }
done
[ "$MOUNTS_OK" = "Y" ] && ok "no orphaned pod mounts"

# 6. driver log scan ----------------------------------------------------------
step "6/7 driver logs since T0"
SINCE=$(rfc3339 "$T0")
LOG_OK=Y
{ kubectl logs -n "$DRIVER_NS" "$(controller_pod)" -c flint-csi-controller --since-time="$SINCE" 2>/dev/null
  for n in $(worker_nodes); do
    p=$(csi_node_pod "$n"); [ -n "$p" ] && kubectl logs -n "$DRIVER_NS" "$p" -c flint-csi-driver --since-time="$SINCE" 2>/dev/null
  done
} > "$ART/driver-logs.txt"
# Errors mentioning our volume in the final 60s = unresolved at drill end.
VOLID=$(kubectl get pv "$PV" -o jsonpath='{.spec.csi.volumeHandle}' 2>/dev/null || echo "$PV")
CUTOFF=$(( $(epoch) - 60 ))
RECENT_ERR=$(grep -iE "error|panic" "$ART/driver-logs.txt" | grep -F "${VOLID:-___}" | tail -20 || true)
if [ -n "$RECENT_ERR" ]; then
  # crude recency filter: driver logs carry RFC3339 timestamps at line start
  LAST_TS=$(echo "$RECENT_ERR" | tail -1 | grep -oE '^[0-9T:.-]+Z' || true)
  if [ -n "$LAST_TS" ]; then
    LAST_EPOCH=$(date -j -u -f "%Y-%m-%dT%H:%M:%SZ" "$(echo "$LAST_TS" | cut -c1-19)Z" +%s 2>/dev/null || echo 0)
    [ "$LAST_EPOCH" -lt "$CUTOFF" ] || { LOG_OK=N; FAILED="$FAILED logs"; note "volume still erroring in final 60s"; }
  fi
fi
[ "$LOG_OK" = "Y" ] && ok "no unresolved volume errors (full scan in $ART/driver-logs.txt)"

# 7. timings + CSV ------------------------------------------------------------
step "7/7 timings"
STALL=$(max_stall_since "$T0")
ok "max ledger stall ${STALL:-?}s"

[ -z "$FAILED" ] && VERDICT=PASS || VERDICT=FAIL
csv_append "$(rfc3339 "$T0"),$PHASE,$DRILL,$T_READY,${STALL:-},$RESTART_DELTA,${PRE_NODE:-},${POST_NODE:-},$VA_OK,$NVME_OK,$MOUNTS_OK,$DB,$LOG_OK,$VERDICT,\"${NOTES}${FAILED:+ failed:$FAILED}\""

printf '\n%s: drill %s/%s — ready=%ss stall=%ss %s\n' "$VERDICT" "$PHASE" "$DRILL" "$T_READY" "${STALL:-?}" "${FAILED:+(failed:$FAILED)}"
[ "$VERDICT" = "PASS" ]
