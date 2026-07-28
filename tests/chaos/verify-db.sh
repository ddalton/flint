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
PG_UP=1
$TMO 30 kubectl exec -n "$NS" $PG -c postgres -- pg_isready -q -U postgres \
  && ok "pg_isready" \
  || { PG_UP=0; FAILED="$FAILED isready"; note "pg_isready FAILED (or 30s timeout)"; }

# 2. ledger reconciliation (acked ⊆ ledger)
LP=$(load_pod)
if [ "$PG_UP" != "1" ]; then
  # An unreachable postgres returns an EMPTY seq list, which comm reads
  # as "every acked write is missing" — a fabricated total-loss verdict
  # (runz 3.6: '666 lost' against a hung pg-0). Loss is UNKNOWN here,
  # not total; isready already failed the drill.
  note "ledger check SKIPPED (pg unreachable — loss unknown, not counted)"
elif [ -n "$LP" ]; then
  # comm needs LEXICOGRAPHIC order — sort -n desynchronizes its merge the
  # moment the two lists are not near-identical (an oracle-pod restart
  # makes acked.log a mid-stream subset), fabricating sparse "missing"
  # seqs that a direct heap probe disproves (phase-2u false FAILs).
  MISSING=$(comm -23 \
    <($TMO 120 kubectl exec -n "$NS" "$LP" -- sh -c 'cut -d" " -f1 /acked/acked.log 2>/dev/null' | sort) \
    <($TMO 120 kubectl exec -n "$NS" $PG -c postgres -- psql -U postgres -d bench -Atqc \
        'SELECT seq FROM ledger' | sort))
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

# 3. physical integrity. -j2 + 1200s budget: the dataset outgrew the
# old 600s single-stream budget once soak/churn history piled up, and
# a TIMEOUT is not a corruption verdict — report the two distinctly
# (a timeout drill row still fails, but says why, and the amcheck
# OUTPUT is preserved for the corruption case instead of >/dev/null).
if [ "${QUICK:-0}" != "1" ]; then
  AMCHECK_OUT=$(mktemp)
  $TMO 1200 kubectl exec -n "$NS" $PG -c postgres -- pg_amcheck -U postgres -d bench --heapallindexed -j 2 >"$AMCHECK_OUT" 2>&1
  AMCHECK_RC=$?
  if [ "$AMCHECK_RC" -eq 0 ]; then
    ok "pg_amcheck clean"
  elif [ "$AMCHECK_RC" -eq 124 ]; then
    FAILED="$FAILED amcheck-timeout"; note "pg_amcheck TIMEOUT at 1200s (integrity NOT verified — not a corruption finding)"
  else
    FAILED="$FAILED amcheck"; note "pg_amcheck CORRUPTION/ERROR (rc=$AMCHECK_RC): $(tail -3 "$AMCHECK_OUT" | tr '\n' ' ')"
  fi
  rm -f "$AMCHECK_OUT"
else
  note "amcheck skipped (QUICK=1)"
fi

# 4. log scan — benign: "was not properly shut down; automatic recovery"
# MUST read --previous too. `kubectl logs` shows only the CURRENT container,
# so a restart or reschedule silently erases findings: on runai 3.6e
# (2026-07-27) kubelet evicted pg-0 for DiskPressure and it came back at
# 23:11:12Z, wiping the 23:05:38 F52 PANIC that had been read live minutes
# earlier — and the drill duly recorded db=PASS on a run that had panicked.
PGPAT='PANIC|checksum verification failed|invalid page|could not read block'
BAD=$(kubectl logs -n "$NS" $PG -c postgres --since-time="$(rfc3339 "$T0")" 2>/dev/null | grep -cE "$PGPAT")
PREV=$(kubectl logs -n "$NS" $PG -c postgres --previous 2>/dev/null | grep -cE "$PGPAT")
# A restarted container is itself a caveat: --previous only reaches back ONE
# instance, so two restarts inside the window can still hide something.
RESTARTED=$(kubectl get pod -n "$NS" $PG -o jsonpath='{range .status.containerStatuses[*]}{.restartCount}{"\n"}{end}' 2>/dev/null \
  | awk '{s+=$1} END{print s+0}')
# BSD date, matching lib.sh's rfc3339(). A rescheduled StatefulSet pod keeps
# its name but resets restartCount to 0 and has no --previous, so startTime is
# the only signal that catches it.
PG_START=$(kubectl get pod -n "$NS" $PG -o jsonpath='{.status.startTime}' 2>/dev/null)
PG_START_EPOCH=$(date -u -jf '%Y-%m-%dT%H:%M:%SZ' "$PG_START" +%s 2>/dev/null || echo 0)
if [ "${BAD:-0}" -eq 0 ] && [ "${PREV:-0}" -eq 0 ]; then
  if [ "${RESTARTED:-0}" -gt 0 ] || [ "${PG_START_EPOCH:-0}" -gt "$T0" ]; then
    note "pglog UNPROVEN, not clean: the pod restarted (${RESTARTED}) or started after T0 (${PG_START:-?}) — earlier instances' logs are unreachable"
  else
    ok "postgres log clean (current + previous instance)"
  fi
else
  FAILED="$FAILED pglog"
  note "$BAD corruption-pattern lines in postgres log (+$PREV in the previous container instance)"
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
