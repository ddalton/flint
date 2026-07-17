#!/usr/bin/env bash
# Data-integrity verdict for the Postgres chaos workload. PASS requires all:
#   1. pg_isready
#   2. ledger reconciliation — every ACKed seq present in the DB
#      (lost acked write = the verdict that matters; synchronous_commit=on)
#   3. pg_amcheck --heapallindexed (torn pages / broken indexes; skip: QUICK=1)
#   4. postgres log clean of PANIC/checksum/invalid-page since T0
#   5. writability probe (5s pgbench)
#
#   ./verify-db.sh [t0-epoch]     (t0 defaults to 15 min ago)
set -uo pipefail
cd "$(dirname "$0")"
. ./lib.sh

T0=${1:-$(( $(epoch) - 900 ))}
need_env
FAILED=""

step "db verdict (since $(rfc3339 "$T0"))"

# Every check is timeout-wrapped: a dead volume hangs psql/amcheck forever
# (1.9b wedged the whole batch inside this script for 20+ min).
TMO=$(command -v timeout || command -v gtimeout)

# 1. up
$TMO 30 kubectl exec -n "$NS" $PG -c postgres -- pg_isready -q -U postgres \
  && ok "pg_isready" || { FAILED="$FAILED isready"; note "pg_isready FAILED (or 30s timeout)"; }

# 2. ledger reconciliation (acked ⊆ ledger)
LP=$(load_pod)
if [ -n "$LP" ]; then
  MISSING=$(comm -23 \
    <($TMO 120 kubectl exec -n "$NS" "$LP" -- sh -c 'cut -d" " -f1 /acked/acked.log 2>/dev/null' | sort -n) \
    <($TMO 120 kubectl exec -n "$NS" $PG -c postgres -- psql -U postgres -d bench -Atqc \
        'SELECT seq FROM ledger ORDER BY seq' | sort -n))
  if [ -z "$MISSING" ]; then
    ACKED=$(kubectl exec -n "$NS" "$LP" -- sh -c 'wc -l < /acked/acked.log 2>/dev/null' | tr -d ' ')
    ok "ledger: all ${ACKED:-0} acked writes present"
  else
    FAILED="$FAILED ledger"
    note "LOST ACKED WRITES: $(echo "$MISSING" | head -5 | tr '\n' ' ')($(echo "$MISSING" | wc -l | tr -d ' ') total)"
  fi
else
  FAILED="$FAILED no-load-pod"
fi

# 3. physical integrity
if [ "${QUICK:-0}" != "1" ]; then
  if $TMO 600 kubectl exec -n "$NS" $PG -c postgres -- pg_amcheck -U postgres -d bench --heapallindexed >/dev/null 2>&1; then
    ok "pg_amcheck clean"
  else
    FAILED="$FAILED amcheck"; note "pg_amcheck FAILED (or 600s timeout)"
  fi
else
  note "amcheck skipped (QUICK=1)"
fi

# 4. log scan — benign: "was not properly shut down; automatic recovery"
BAD=$(kubectl logs -n "$NS" $PG -c postgres --since-time="$(rfc3339 "$T0")" 2>/dev/null \
  | grep -cE 'PANIC|checksum verification failed|invalid page|could not read block')
if [ "${BAD:-0}" -eq 0 ]; then ok "postgres log clean"; else
  FAILED="$FAILED pglog"; note "$BAD corruption-pattern lines in postgres log"
fi

# 5. writability
if [ -n "$LP" ] && $TMO 60 kubectl exec -n "$NS" "$LP" -- pgbench -n -c 2 -T 5 bench >/dev/null 2>&1; then
  ok "writability probe"
else
  FAILED="$FAILED write-probe"; note "writability probe FAILED"
fi

if [ -z "$FAILED" ]; then
  printf '\nDB-VERDICT: PASS\n'; exit 0
else
  printf '\nDB-VERDICT: FAIL (%s)\n' "$FAILED"; exit 1
fi
