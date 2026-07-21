#!/usr/bin/env bash
# Phase 1 drills — RWO, numReplicas=1 (SC flint). One drill per invocation:
#
#   AWS_PROFILE=rolesanywhere KUBECONFIG=... ./phase1.sh 1.2
#
# Ordered gentle → brutal; ☠ drills (1.13/1.14/1.15) destroy the harness
# volume by design and re-deploy it afterwards. Every drill ends with
# verify-drill.sh (7-point checklist + results.csv row).
set -uo pipefail
cd "$(dirname "$0")/.."
. ./lib.sh

DRILL=${1:?drill id, e.g. 1.2}
# PHASE_LABEL lets the Phase-2 regression subset reuse these drills against a
# harness deployed on flint-r2 (PHASE_LABEL=2 ./drills/phase1.sh 1.4).
PHASE_LABEL=${PHASE_LABEL:-1}

pre() {
  need_env
  harness_healthy
  PRE_NODE=$(pg_node); PRE_UID=$(pg_uid); PRE_RESTARTS=$(pg_restarts)
  PRE_ORPHANS=$(orphan_data_paths | grep -c . || true)
  export PRE_NODE PRE_UID PRE_RESTARTS PRE_ORPHANS
  T0=$(epoch)
  step "T0=$T0 pg-0 on $PRE_NODE (uid ${PRE_UID:0:8}, restarts $PRE_RESTARTS, orphans $PRE_ORPHANS)"
}

verify() { # [env overrides passed by caller]
  ./verify-drill.sh "$PHASE_LABEL" "$DRILL" "$T0"
}

wait_acks_fresh() { # [budget_s] — until the ledger acks something NEWER than T0
  # (age-only checking races: the final pre-kill ack looks "fresh" for 5s,
  #  which let 1.9b record a bogus io_resume while I/O was actually dead)
  local budget=${1:-180} last now i
  for i in $(seq 1 $(( budget / 5 ))); do
    last=$(timeout 15 kubectl exec -n "$NS" "$(load_pod)" -- sh -c 'tail -1 /acked/acked.log 2>/dev/null' | awk '{print $2}')
    now=$(epoch)
    [ -n "$last" ] && [ "$last" -gt "${T0:-0}" ] && [ $(( now - last )) -lt 5 ] && return 0
    sleep 5
  done
  return 1
}

spdk_restarts() { # <node>
  # spdk-tgt is a native-sidecar INIT container: it reports under
  # initContainerStatuses, never containerStatuses.
  kubectl get pod -n "$DRIVER_NS" "$(csi_node_pod "$1")" \
    -o jsonpath='{.status.initContainerStatuses[?(@.name=="spdk-tgt")].restartCount}' 2>/dev/null
}

CORDONED=""; TAINTED=""; DEAD_IID=""
restore() {
  set +e
  [ -n "$CORDONED" ] && kubectl uncordon "$CORDONED" >/dev/null 2>&1
  [ -n "$TAINTED" ] && untaint_oos "$TAINTED"
  [ -n "$DEAD_IID" ] && kubelet_start_ssm "$DEAD_IID"
}
trap restore EXIT

case "$DRILL" in

1.1) # in-container postmaster SIGKILL — no CSI involvement expected
  pre
  kubectl exec -n "$NS" $PG -c chaos -- pkill -9 -x postgres
  note "postmaster killed; expecting in-place container restart"
  sleep 5
  EXPECT_RESCHEDULE=none READY_TIMEOUT=120 verify
  # extra: assert zero unstage/unpublish for our volume (container restart
  # must not touch the mount)
  PV=$(pg_pv)
  CSI_CALLS=$(kubectl logs -n "$DRIVER_NS" "$(csi_node_pod "$PRE_NODE")" -c flint-csi-driver \
    --since-time="$(rfc3339 "$T0")" 2>/dev/null | grep -c "NodeUnstageVolume\|NodeUnpublishVolume" || true)
  [ "${CSI_CALLS:-0}" -eq 0 ] && ok "zero CSI unmount calls" || note "WARN: $CSI_CALLS CSI unmount calls seen"
  ;;

1.2) # graceful pod delete — full same/any-node publish cycle
  pre
  kubectl delete pod -n "$NS" $PG --wait=false
  wait_pod_replaced "$NS" $PG "$PRE_UID" 300 || fail "replacement never Ready"
  # clean-shutdown assert: crash recovery in the log would mean SIGINT never
  # reached postgres before unmount
  if kubectl logs -n "$NS" $PG -c postgres --since-time="$(rfc3339 "$T0")" 2>/dev/null \
      | grep -q "was not properly shut down"; then
    NOTES="DIRTY SHUTDOWN on graceful delete" verify
  else
    NOTES="clean shutdown" verify
  fi
  ;;

1.3) # force delete — unmount under a dirty postmaster
  pre
  kubectl delete pod -n "$NS" $PG --grace-period=0 --force --wait=false
  wait_pod_replaced "$NS" $PG "$PRE_UID" 300 || fail "replacement never Ready"
  NOTES="force delete; WAL replay expected" verify
  ;;

1.4) # cordon + delete → cross-node reschedule
  pre
  kubectl cordon "$PRE_NODE" >/dev/null; CORDONED="$PRE_NODE"
  kubectl delete pod -n "$NS" $PG --wait=false
  wait_pod_replaced "$NS" $PG "$PRE_UID" 400 || fail "replacement never Ready"
  kubectl uncordon "$PRE_NODE" >/dev/null; CORDONED=""
  # old node must have dropped its nvme session for our volume
  EXPECT_RESCHEDULE=cross NOTES="cross-node migration" verify
  ;;

1.5) # node drain (Eviction API)
  pre
  kubectl drain "$PRE_NODE" --ignore-daemonsets --delete-emptydir-data --timeout=180s >/dev/null 2>&1 &
  DRAIN_PID=$!
  wait_pod_replaced "$NS" $PG "$PRE_UID" 400 || fail "replacement never Ready"
  wait $DRAIN_PID 2>/dev/null
  kubectl uncordon "$PRE_NODE" >/dev/null
  EXPECT_RESCHEDULE=cross NOTES="drain" verify
  ;;

1.6) # controller kill mid-ATTACH
  pre
  kubectl cordon "$PRE_NODE" >/dev/null; CORDONED="$PRE_NODE"
  kubectl delete pod -n "$NS" $PG --wait=false
  sleep "${CTRL_KILL_DELAY:-4}"   # old-node detach done-ish, new-node attach in flight
  kubectl delete pod -n "$DRIVER_NS" "$(controller_pod)" --wait=false
  note "controller killed ${CTRL_KILL_DELAY:-4}s after pod delete"
  wait_pod_replaced "$NS" $PG "$PRE_UID" 500 || fail "replacement never Ready"
  kubectl uncordon "$PRE_NODE" >/dev/null; CORDONED=""
  EXPECT_RESCHEDULE=cross READY_TIMEOUT=500 NOTES="controller killed mid-attach" verify
  ;;

1.7) # controller kill mid-DETACH
  pre
  kubectl delete pod -n "$NS" $PG --wait=false
  sleep "${CTRL_KILL_DELAY:-3}"
  kubectl delete pod -n "$DRIVER_NS" "$(controller_pod)" --wait=false
  note "controller killed ${CTRL_KILL_DELAY:-3}s after pod delete (detach in flight)"
  wait_pod_replaced "$NS" $PG "$PRE_UID" 500 || fail "replacement never Ready"
  READY_TIMEOUT=500 NOTES="controller killed mid-detach" verify
  ;;

1.8) # controller ABSENT through a migration; queued ops replay on return
  pre
  kubectl scale deploy/flint-csi-controller -n "$DRIVER_NS" --replicas=0 >/dev/null
  kubectl wait --for=delete pod -l app=flint-csi-controller -n "$DRIVER_NS" --timeout=60s >/dev/null 2>&1
  kubectl cordon "$PRE_NODE" >/dev/null; CORDONED="$PRE_NODE"
  kubectl delete pod -n "$NS" $PG --wait=false
  note "controller at 0; migration queued for 60s"
  sleep 60
  kubectl scale deploy/flint-csi-controller -n "$DRIVER_NS" --replicas=1 >/dev/null
  wait_pod_replaced "$NS" $PG "$PRE_UID" 500 || fail "replacement never Ready after controller return"
  kubectl uncordon "$PRE_NODE" >/dev/null; CORDONED=""
  EXPECT_RESCHEDULE=cross READY_TIMEOUT=500 NOTES="controller scaled 0 for 60s mid-migration" verify
  ;;

1.9) # spdk-tgt hard kill on pg's node — v1.15.0 graceful recovery
  pre
  IID=$(instance_id_for_node "$PRE_NODE")
  [ -n "$IID" ] || fail "no instance id for $PRE_NODE"
  SPDK_PRE=$(spdk_restarts "$PRE_NODE")
  # SPDK renames its main thread to reactor_0, so comm-matching (-x) never
  # hits; match the cmdline (the hosts have no crictl, so no fallback).
  ssm_run "$IID" "pkill -9 -f /usr/local/bin/spdk_tgt" >/dev/null
  note "spdk-tgt SIGKILL sent via SSM to $IID"
  for i in $(seq 1 24); do
    [ "$(spdk_restarts "$PRE_NODE")" != "$SPDK_PRE" ] && break; sleep 5
  done
  [ "$(spdk_restarts "$PRE_NODE")" != "$SPDK_PRE" ] || fail "spdk-tgt never restarted — kill failed?"
  ok "spdk-tgt container restarted"
  wait_acks_fresh 180 && T_RESUME=$(( $(epoch) - T0 )) || T_RESUME=-1
  [ "$T_RESUME" -ge 0 ] && ok "I/O resumed ${T_RESUME}s after kill" || note "I/O did NOT resume in 180s"
  EXPECT_RESCHEDULE=none READY_TIMEOUT=180 NOTES="spdk-tgt hard kill; io_resume=${T_RESUME}s" verify
  ;;

1.9b) # full csi-node POD delete on pg's node — node-agent restarts too
      # (hypothesis per v1.15.0 known limit: this may trip the landmine for
      # a pre-existing r1 mount; drill RECORDS actual behavior)
  pre
  CNP=$(csi_node_pod "$PRE_NODE")
  kubectl delete pod -n "$DRIVER_NS" "$CNP" --wait=false
  note "csi-node pod $CNP deleted (node-agent + spdk-tgt both restart)"
  kubectl wait --for=condition=Ready pod -l app=flint-csi-node -n "$DRIVER_NS" \
    --field-selector "spec.nodeName=$PRE_NODE" --timeout=180s >/dev/null 2>&1
  wait_acks_fresh 300 && T_RESUME=$(( $(epoch) - T0 )) || T_RESUME=-1
  READY_TIMEOUT=600 NOTES="csi-node POD delete (landmine probe); io_resume=${T_RESUME}s" verify
  ;;

1.10) # churn ×20 — leak detection
  pre
  : > "$ARTIFACTS/churn-cycles.txt"
  for i in $(seq 1 20); do
    CUR_NODE=$(pg_node); CUR_UID=$(pg_uid); C0=$(epoch)
    if [ $(( i % 2 )) -eq 0 ]; then
      kubectl cordon "$CUR_NODE" >/dev/null; CORDONED="$CUR_NODE"
    fi
    kubectl delete pod -n "$NS" $PG --wait=false
    wait_pod_replaced "$NS" $PG "$CUR_UID" 300 || fail "cycle $i: replacement never Ready"
    [ -n "$CORDONED" ] && { kubectl uncordon "$CORDONED" >/dev/null; CORDONED=""; }
    echo "cycle=$i mode=$([ $(( i % 2 )) -eq 0 ] && echo cross || echo same) secs=$(( $(epoch) - C0 )) node=$(pg_node)" \
      | tee -a "$ARTIFACTS/churn-cycles.txt"
  done
  # leak totals: exactly 1 nvme session / 1 globalmount / 1 VA across fleet
  TOT_GM=0
  for n in $(worker_nodes); do TOT_GM=$(( TOT_GM + $(globalmounts "$n") )); done
  N_VA=$(kubectl get volumeattachments --no-headers 2>/dev/null | wc -l | tr -d ' ')
  [ "$TOT_GM" -eq 1 ] || note "LEAK? total globalmounts=$TOT_GM (want 1)"
  [ "$N_VA" -eq 1 ] || note "LEAK? volumeattachments=$N_VA (want 1)"
  NOTES="churn x20; tot_gm=$TOT_GM vas=$N_VA" verify
  ;;

1.11) # kubelet stop, NO taint — measure the organic slow path
  pre
  IID=$(instance_id_for_node "$PRE_NODE")
  [ -n "$IID" ] || fail "no instance id for $PRE_NODE"
  kubelet_stop "$PRE_NODE"; DEAD_IID="$IID"
  wait_node_notready "$PRE_NODE" 180 || fail "node never NotReady"
  T_NOTREADY=$(( $(epoch) - T0 )); ok "NotReady at ${T_NOTREADY}s"
  PV=$(pg_pv)
  # watch: pod eviction (deletionTimestamp), then attacher force-detach
  T_EVICT=-1; T_DETACH=-1
  for i in $(seq 1 90); do
    [ "$T_EVICT" -lt 0 ] && [ -n "$(kubectl get pod -n "$NS" $PG -o jsonpath='{.metadata.deletionTimestamp}' 2>/dev/null)" ] \
      && { T_EVICT=$(( $(epoch) - T0 )); note "pod eviction (Terminating) at ${T_EVICT}s"; }
    [ -z "$(va_for_pv "$PV")" ] && { T_DETACH=$(( $(epoch) - T0 )); break; }
    sleep 10
  done
  [ "$T_DETACH" -ge 0 ] && ok "VA force-detached at ${T_DETACH}s (no surgery)" \
                        || note "VA NOT detached after 15min — surgery would be required"
  # complete recovery with the sanctioned taint (pod is stuck Terminating)
  taint_oos "$PRE_NODE"; TAINTED="$PRE_NODE"
  wait_pod_replaced "$NS" $PG "$PRE_UID" 600 || fail "replacement never Ready after taint"
  READY_TIMEOUT=60 EXPECT_RESCHEDULE=cross \
    NOTES="kubelet-stop slow path: notready=${T_NOTREADY}s evict=${T_EVICT}s va_detach=${T_DETACH}s then taint" verify
  untaint_oos "$PRE_NODE"; TAINTED=""
  kubelet_start_ssm "$IID"; DEAD_IID=""
  wait_node_ready "$PRE_NODE" 300 && ok "node restored" || note "node not yet Ready — check kubelet"
  ;;

1.12) # kubelet stop + immediate out-of-service taint (fast sanctioned path)
  pre
  IID=$(instance_id_for_node "$PRE_NODE")
  [ -n "$IID" ] || fail "no instance id for $PRE_NODE"
  kubelet_stop "$PRE_NODE"; DEAD_IID="$IID"
  wait_node_notready "$PRE_NODE" 180 || fail "node never NotReady"
  T_NOTREADY=$(( $(epoch) - T0 ))
  taint_oos "$PRE_NODE"; TAINTED="$PRE_NODE"
  wait_pod_replaced "$NS" $PG "$PRE_UID" 400 || fail "replacement never Ready"
  READY_TIMEOUT=60 EXPECT_RESCHEDULE=cross \
    NOTES="kubelet-stop+taint: notready=${T_NOTREADY}s" verify
  untaint_oos "$PRE_NODE"; TAINTED=""
  kubelet_start_ssm "$IID"; DEAD_IID=""
  wait_node_ready "$PRE_NODE" 300 && ok "node restored" || note "node not yet Ready — check kubelet"
  ;;

1.13) # ☠ guest-initiated shutdown (spot-reclaim analog). r1 data on the
      # instance store is LOST BY DESIGN: PASS = clean failure + reset works.
  pre
  IID=$(instance_id_for_node "$PRE_NODE")
  [ -n "$IID" ] || fail "no instance id for $PRE_NODE"
  ssm_run "$IID" "shutdown -h now" >/dev/null
  note "shutdown sent to $IID — waiting for stop"
  aws ec2 wait instance-stopped --region "$AWS_REGION" --instance-ids "$IID"
  wait_node_notready "$PRE_NODE" 180
  taint_oos "$PRE_NODE"; TAINTED="$PRE_NODE"
  # replacement pod MUST fail to stage (backing lvol died with the NVMe)
  sleep 120
  PHASE_STATE=$(kubectl get pod -n "$NS" $PG -o jsonpath='{.status.phase}' 2>/dev/null)
  note "replacement pod state after 120s: ${PHASE_STATE:-gone} (expected NOT Running)"
  kubectl get events -n "$NS" --field-selector involvedObject.name=$PG \
    --sort-by=.lastTimestamp 2>/dev/null | tail -5
  # clean-failure assertions: harness must tear down without surgery
  SC=flint MODE=RWO ./deploy-harness.sh down || fail "teardown after data loss NOT clean (finding!)"
  ok "clean failure: teardown succeeded with dead backing volume"
  untaint_oos "$PRE_NODE"; TAINTED=""
  aws ec2 start-instances --region "$AWS_REGION" --instance-ids "$IID" >/dev/null
  aws ec2 wait instance-running --region "$AWS_REGION" --instance-ids "$IID"
  wait_node_ready "$PRE_NODE" 600 || note "node not Ready after restart — check kubelet/agent"
  note "instance-store wiped on stop/start: watch node agent re-init + any ghost-lvol fallout"
  csv_append "$(rfc3339 "$T0"),$PHASE_LABEL,1.13,-,-,-,$PRE_NODE,-,-,-,-,-,-,PASS,\"clean-failure drill: teardown clean, node restored\""
  SC=flint MODE=RWO ./deploy-harness.sh up
  ;;

1.14) # ☠ EC2 terminate of pg's node. Node is GONE — replacement worker gets
      # scaled in via trove afterwards (POST /servers/create works on a
      # deployed project).
  pre
  IID=$(instance_id_for_node "$PRE_NODE")
  [ -n "$IID" ] || fail "no instance id for $PRE_NODE"
  aws ec2 terminate-instances --region "$AWS_REGION" --instance-ids "$IID" >/dev/null
  note "terminated $IID"
  wait_node_notready "$PRE_NODE" 300
  taint_oos "$PRE_NODE"; TAINTED="$PRE_NODE"
  sleep 120
  kubectl get events -n "$NS" --field-selector involvedObject.name=$PG \
    --sort-by=.lastTimestamp 2>/dev/null | tail -5
  SC=flint MODE=RWO ./deploy-harness.sh down || fail "teardown after node loss NOT clean (finding!)"
  ok "clean failure: teardown succeeded after node termination"
  untaint_oos "$PRE_NODE"; TAINTED=""
  kubectl delete node "$PRE_NODE" >/dev/null 2>&1
  csv_append "$(rfc3339 "$T0"),$PHASE_LABEL,1.14,-,-,-,$PRE_NODE,-,-,-,-,-,-,PASS,\"node terminate: teardown clean; scale replacement via trove\""
  note "NEXT: scale replacement worker via trove, then deploy-harness.sh up"
  ;;

1.15) # ☠ full csi-node DS roll — known limit; expected-fail on continuity.
      # PASS = documented behavior reproduced + recovery via pod bounce clean.
  pre
  kubectl rollout restart ds/flint-csi-node -n "$DRIVER_NS" >/dev/null
  kubectl rollout status ds/flint-csi-node -n "$DRIVER_NS" --timeout=600s >/dev/null 2>&1
  ok "DS rolled"
  if wait_acks_fresh 240; then
    NOTES="DS roll: I/O survived (better than documented limit!)" verify
  else
    note "I/O dead post-roll (documented landmine) — recovering via pod bounce"
    RUID=$(pg_uid)
    kubectl delete pod -n "$NS" $PG --grace-period=0 --force --wait=false
    wait_pod_replaced "$NS" $PG "$RUID" 400 || fail "bounce recovery failed"
    wait_acks_fresh 180 || fail "I/O never resumed after bounce"
    READY_TIMEOUT=120 NOTES="DS roll landmine reproduced; recovery=pod bounce" verify
  fi
  ;;

*) fail "unknown drill '$DRILL'" ;;
esac
