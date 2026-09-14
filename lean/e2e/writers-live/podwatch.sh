#!/usr/bin/env bash
# podwatch.sh — E4, cluster events and pod states (plan §2a), plus the E6
# history upload. Run on the CONTROL-PLANE host as root:
#
#   BUCKET=<drill bucket> podwatch.sh start|flush|stop|status
#
# start   detached loops (setsid + nohup, pidfiles; shipper.sh's machinery):
#   events    `kubectl get events -A -w -o json`, one compact line per event
#             (managedFields stripped, observed_ms added) to $EVID/events.jsonl;
#             restarted when kubectl exits (a watch ends on its own; events
#             expire after an hour, which is why this runs from the start)
#   pods      every PODS_SECS (60) `kubectl get pods -A -o json`, one line per
#             pod to $EVID/pods.jsonl: {ts_ms, src:"snap", ns, name, uid, node,
#             phase, reason, deleted, created, restarts, lastState, annotations
#             (incl. chert.us/tenant-pod), labels, owner, containers[...]}
#   podwatch  `kubectl get pods -A -w -o json`, the same compact line with
#             src:"watch", to $EVID/pods-watch.jsonl — a worker pod that lives
#             less than one snapshot interval (a leg-A4 replacement, a
#             crashlooping worker) is still mapped to its tenant
#   upload    every UPLOAD_SECS (300) the flush below
# flush   $EVID -> s3://$BUCKET/_rig/evidence/cp/ and, when $HISTORY exists,
#         $HISTORY -> s3://$BUCKET/_rig/history/ (the sampler's per-leg
#         directories), each with a SHA256SUMS uploaded last; prints each
#         prefix's object count and bytes.
# stop    stops the loops, then flushes.
# status  each loop alive or dead; line counts; the last flushes.
#
# Environment: BUCKET (required), KUBECONFIG (/etc/kubernetes/admin.conf),
# EVID (/mnt/nvme/podwatch/evidence), STATE (/mnt/nvme/podwatch/state),
# HISTORY (/mnt/nvme/history), REGION (us-west-1), PODS_SECS, UPLOAD_SECS,
# KUBECTL_BIN, AWS_BIN, ALLOW_ROOTFS. EVID and STATE differ from the node
# shipper's defaults, so both run on the CP without sharing a stage or lock.
#
# Writes: only under $EVID and $STATE, and only to s3://$BUCKET/_rig/evidence/cp/
# and s3://$BUCKET/_rig/history/.
set -u

PW_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
PW_SELF="$PW_DIR/$(basename "${BASH_SOURCE[0]}")"
export KUBECONFIG=${KUBECONFIG:-/etc/kubernetes/admin.conf}
export EVID=${EVID:-/mnt/nvme/podwatch/evidence}
export STATE=${STATE:-/mnt/nvme/podwatch/state}
export HISTORY=${HISTORY:-/mnt/nvme/history}
export PODS_SECS=${PODS_SECS:-60}
export UPLOAD_SECS=${UPLOAD_SECS:-300}
export KUBECTL_BIN=${KUBECTL_BIN:-kubectl}

[ -f "$PW_DIR/shipper.sh" ] || { echo "podwatch.sh needs shipper.sh beside it ($PW_DIR)" >&2; exit 1; }
SHIPPER_LIB_ONLY=1
# shellcheck source=shipper.sh
. "$PW_DIR/shipper.sh"

PW_LOOPS="events pods podwatch upload"

pw_flush() {
    local rc=0
    rig_flush "$STATE/stage-evidence" "s3://$BUCKET/_rig/evidence/cp/" cp "$EVID" "$STATE/logs=_podwatch/logs" || rc=1
    if [ -d "$HISTORY" ]; then
        rig_flush "$STATE/stage-history" "s3://$BUCKET/_rig/history/" history "$HISTORY" || rc=1
    else
        say "flush history: $HISTORY does not exist — nothing to upload"
    fi
    return $rc
}

pw_run_stream() { # kubectl-noun kind out
    trap 'exit 0' TERM
    while :; do
        "$KUBECTL_BIN" get "$1" -A -w -o json 2>>"$STATE/logs/kubectl-$1.err" |
            python3 -u "$STATE/shipper_loops.py" jsonstream "$2" watch >>"$3"
        say "kubectl get $1 -w ended; restarting" >>"$STATE/logs/kubectl-$1.err"
        sleep 2
    done
}

pw_run_pods() {
    trap 'exit 0' TERM
    local snap
    while :; do
        snap="$STATE/pods-snap.json"
        if "$KUBECTL_BIN" get pods -A -o json >"$snap" 2>>"$STATE/logs/kubectl-pods-snap.err"; then
            python3 "$STATE/shipper_loops.py" jsonstream pod snap <"$snap" >>"$EVID/pods.jsonl"
        else
            say "kubectl get pods failed" >>"$STATE/logs/kubectl-pods-snap.err"
        fi
        sleep "$PODS_SECS" &
        wait $!
    done
}

pw_run_upload() {
    trap 'exit 0' TERM
    while :; do
        sleep "$UPLOAD_SECS" &
        wait $!
        pw_flush || say "periodic flush failed; next in $UPLOAD_SECS s" >&2
    done
}

pw_start_loop() {
    if rig_alive "$1"; then
        say "$1: already running (pid $(cat "$(rig_pidfile "$1")"))"
        return 0
    fi
    rig_detach "$1" bash "$PW_SELF" _run "$1"
    say "$1: started (pid $(cat "$(rig_pidfile "$1")"))"
}

pw_check() {
    rig_require_name "${BUCKET:-}" BUCKET
    rig_require_offroot "$EVID" "$STATE"
    mkdir -p "$EVID" "$STATE/pids" "$STATE/logs"
}

case "${1:-}" in
    start)
        pw_check
        "$KUBECTL_BIN" version >/dev/null 2>&1 || say "WARNING: kubectl cannot reach the API with KUBECONFIG=$KUBECONFIG" >&2
        rig_write_loops_py
        for l in $PW_LOOPS; do pw_start_loop "$l"; done
        ;;
    flush)
        pw_check
        [ -f "$STATE/shipper_loops.py" ] || rig_write_loops_py
        pw_flush
        ;;
    stop)
        pw_check
        [ -f "$STATE/shipper_loops.py" ] || rig_write_loops_py
        for l in $PW_LOOPS; do rig_kill_loop "$l"; done
        pw_flush
        ;;
    status)
        rc=0
        for l in $PW_LOOPS; do
            if rig_alive "$l"; then say "$l: alive (pid $(cat "$(rig_pidfile "$l")"))"; else say "$l: DEAD"; rc=1; fi
        done
        for f in events.jsonl pods.jsonl pods-watch.jsonl; do
            say "$f: $(wc -l <"$EVID/$f" 2>/dev/null | tr -d ' ' || echo 0) lines"
        done
        say "last flush cp: $(cat "$STATE/last-flush-cp" 2>/dev/null || echo never); history: $(cat "$STATE/last-flush-history" 2>/dev/null || echo never)"
        exit $rc
        ;;
    _run)
        case "${2:-}" in
            events) pw_run_stream events event "$EVID/events.jsonl" ;;
            podwatch) pw_run_stream pods pod "$EVID/pods-watch.jsonl" ;;
            pods) pw_run_pods ;;
            upload) pw_run_upload ;;
            *) die "unknown loop ${2:-}" ;;
        esac
        ;;
    *)
        sed -n '2,33p' "$PW_SELF" >&2
        exit 2
        ;;
esac
