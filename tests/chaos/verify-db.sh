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
  # The ACKED set is read FIRST and the table AFTER it, never in parallel.
  # The load keeps inserting, so a table snapshot taken before the newest
  # acks (or a truncated psql/exec stream, which the old pipeline could not
  # see) reads exactly the newest rows as lost: the 1.61.3 gate's 2.2a
  # (2026-10-30) failed on seqs 615-619, and drill 1.9 found all 1151 acked
  # rows present three minutes later on the same harness. A seq missing from
  # the full read is then asked for by value; only a seq still absent is a
  # lost write. Either read failing is "loss unknown", never a pass.
  # comm needs LEXICOGRAPHIC order -- sort -n desynchronizes its merge the
  # moment the two lists are not near-identical (an oracle-pod restart
  # makes acked.log a mid-stream subset), fabricating sparse "missing"
  # seqs that a direct heap probe disproves (phase-2u false FAILs).
  ED=${EVIDENCE_DIR:-$(mktemp -d)}; mkdir -p "$ED"
  if ! $TMO 120 kubectl exec -n "$NS" "$LP" -- sh -c 'cat /acked/acked.log' > "$ED/acked.log" 2>"$ED/acked.err"; then
    FAILED="$FAILED ledger-unread"; note "ledger UNREAD: acked.log not readable (loss unknown; $ED/acked.err)"
  elif ! $TMO 120 kubectl exec -n "$NS" $PG -c postgres -- psql -U postgres -d bench -v ON_ERROR_STOP=1 -Atqc \
      'SELECT seq FROM ledger' > "$ED/ledger-seqs.txt" 2>"$ED/ledger.err"; then
    FAILED="$FAILED ledger-unread"; note "ledger UNREAD: the table query failed (loss unknown; $ED/ledger.err)"
  else
    MISSING=$(comm -23 <(cut -d" " -f1 "$ED/acked.log" | sort) <(sort "$ED/ledger-seqs.txt"))
    ACKED=$(grep -c . "$ED/acked.log")
    if [ -z "$MISSING" ]; then
      ok "ledger: all ${ACKED:-0} acked writes present"
    else
      echo "$MISSING" > "$ED/missing-first-read.txt"
      LIST=$(echo "$MISSING" | head -2000 | paste -sd, -)
      if ! $TMO 60 kubectl exec -n "$NS" $PG -c postgres -- psql -U postgres -d bench -v ON_ERROR_STOP=1 -Atqc \
          "SELECT seq FROM ledger WHERE seq IN ($LIST)" > "$ED/missing-requery.txt" 2>>"$ED/ledger.err"; then
        FAILED="$FAILED ledger-unread"
        note "ledger UNREAD: $(echo "$MISSING" | wc -l | tr -d ' ') seqs absent from the full read and the re-query failed (loss unknown)"
      else
        STILL=$(comm -23 <(echo "$MISSING" | sort) <(sort "$ED/missing-requery.txt"))
        if [ -z "$STILL" ]; then
          ok "ledger: all ${ACKED:-0} acked writes present ($(echo "$MISSING" | wc -l | tr -d ' ') absent from the full read were found by value: a read artifact, not a loss)"
        else
          echo "$STILL" > "$ED/lost.txt"
          FAILED="$FAILED ledger"
          note "LOST ACKED WRITES: $(echo "$STILL" | head -5 | tr '\n' ' ')($(echo "$STILL" | wc -l | tr -d ' ') total, absent on a by-value re-query; $ED)"
          # the ledger's own view of the neighbourhood: ctid + xmin of the rows
          # around each lost seq (which heap pages hold them, which xacts wrote them)
          LO=$(echo "$STILL" | sort -n | head -1); HI=$(echo "$STILL" | sort -n | tail -1)
          $TMO 60 kubectl exec -n "$NS" $PG -c postgres -- psql -U postgres -d bench -Atqc \
            "SELECT seq, ctid, xmin, ts FROM ledger WHERE seq BETWEEN $((LO - 50)) AND $((HI + 50)) ORDER BY seq" \
            > "$ED/ledger-around-lost.txt" 2>&1
        fi
      fi
    fi
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
# Keep postgres's own record of the window (a FAIL is examined after the
# harness is gone), and say whether it ran crash recovery in place: the
# postmaster reinitializes without a container restart, and the pattern
# scan below does not look for recovery.
if [ -n "${EVIDENCE_DIR:-}" ]; then
  kubectl logs -n "$NS" $PG -c postgres --timestamps --since-time="$(rfc3339 "$T0")" > "$EVIDENCE_DIR/pg.log" 2>&1
  kubectl logs -n "$NS" $PG -c postgres --timestamps --previous > "$EVIDENCE_DIR/pg-previous.log" 2>/dev/null \
    || rm -f "$EVIDENCE_DIR/pg-previous.log"
  [ -n "${LP:-}" ] && kubectl logs -n "$NS" "$LP" --timestamps --since-time="$(rfc3339 "$T0")" > "$EVIDENCE_DIR/pg-load.log" 2>&1
fi
RECOV=$(kubectl logs -n "$NS" $PG -c postgres --since-time="$(rfc3339 "$T0")" 2>/dev/null \
  | grep -cE 'terminating any other active server processes|database system was interrupted|redo starts at|was terminated by signal')
[ "${RECOV:-0}" -eq 0 ] || note "postgres ran crash recovery in place since T0 ($RECOV lines; not a verdict by itself)"
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
