#!/usr/bin/env bash
# Phase 3 drills — RWX (NFS). Harness: SC=flint MODE=RWX WITNESS=1 (3.6 wants
# SC=flint-r2). Postgres stays the single writer; the witness Deployment on a
# different node proves true multi-node access through every drill.
#
#   AWS_PROFILE=rolesanywhere KUBECONFIG=... ./phase3.sh 3.2
#
#   3.1   graceful cross-node pg migration — exactly ONE nfs pod throughout
#   3.1b  force delete + in-container pkill -9 postgres (dirty postmaster/NFS)
#   3.2   flint-nfs pod delete → liveness reconciler recreates ≤~45s
#   3.3a  spdk-tgt PROCESS kill on the nfs-server's node (validated vector)
#   3.3b  csi-node POD delete on the nfs-server's node (F8 probe;
#         recovery = delete nfs pod → reconciler recreates + fresh stage)
#   3.4   csi-node POD delete on the CLIENT (pg) node → no stall expected
#   3.5   controller kill mid-ControllerPublish of a fresh RWX attach —
#         no duplicate nfs pods
#   3.6   nfs-server NODE kill on an r2 volume (needs SC=flint-r2 harness)
#   3.7   client node kill (kubelet stop + taint)
#   3.8   client churn ×10 — nfs pod must survive untouched (same UID)
#   3.9 ☠ full csi-node DS roll (documented-limit drill, run last)
set -uo pipefail
cd "$(dirname "$0")/.."
. ./lib.sh

DRILL=${1:?drill id, e.g. 3.2}
PHASE_LABEL=${PHASE_LABEL:-3}

# ---- RWX topology helpers ----------------------------------------------

volume_handle() { kubectl get pv "$(pg_pv)" -o jsonpath='{.spec.csi.volumeHandle}' 2>/dev/null; }

nfs_pod() { # exact name is flint-nfs-<volumeHandle>
  local h; h=$(volume_handle)
  kubectl get pod -n "$DRIVER_NS" "flint-nfs-$h" -o jsonpath='{.metadata.name}' 2>/dev/null
}
nfs_pod_uid()  { kubectl get pod -n "$DRIVER_NS" "$(nfs_pod)" -o jsonpath='{.metadata.uid}' 2>/dev/null; }
nfs_node()     { kubectl get pod -n "$DRIVER_NS" "$(nfs_pod)" -o jsonpath='{.spec.nodeName}' 2>/dev/null; }
nfs_pod_count() { # pods for this volume (want exactly 1, always)
  local h; h=$(volume_handle)
  kubectl get pods -n "$DRIVER_NS" --no-headers 2>/dev/null | grep -c "^flint-nfs-$h " || true
}

witness_pod() { kubectl get pod -n "$NS" -l app=witness --field-selector status.phase=Running -o jsonpath='{.items[0].metadata.name}' 2>/dev/null; }
witness_verdict() { # <t0> — mismatches since t0 + freshness of its shared-file writes
  local wp mism last
  wp=$(witness_pod)
  [ -n "$wp" ] || { note "WITNESS MISSING"; return 1; }
  mism=$(kubectl logs -n "$NS" "$wp" --since-time="$(rfc3339 "$1")" 2>/dev/null | grep -c WITNESS-MISMATCH || true)
  # timeout is load-bearing: `tail` on a dead NFS mount blocks in
  # D-state and an un-wrapped exec hangs the whole drill (3.6 hung 87
  # minutes on exactly this line when the witness's server was an
  # orphaned instance — F33). A timed-out read = witness NOT fresh.
  last=$(timeout 15 kubectl exec -n "$NS" "$wp" -- sh -c 'tail -1 /mnt/witness.log' 2>/dev/null | awk '{print $2}')
  local age=$(( $(epoch) - ${last:-0} ))
  if [ "${mism:-0}" -eq 0 ] && [ "$age" -lt 15 ]; then
    ok "witness clean (0 mismatches, last write ${age}s ago)"; return 0
  fi
  note "WITNESS: mismatches=$mism last-write-age=${age}s"; return 1
}

spdk_restarts() { # <node> — spdk-tgt is a native-sidecar INIT container
  kubectl get pod -n "$DRIVER_NS" "$(csi_node_pod "$1")" \
    -o jsonpath='{.status.initContainerStatuses[?(@.name=="spdk-tgt")].restartCount}' 2>/dev/null
}

wait_acks_fresh() { # [budget_s] — ledger acks something NEWER than T0
  local budget=${1:-180} last now i
  for i in $(seq 1 $(( budget / 5 ))); do
    last=$(timeout 15 kubectl exec -n "$NS" "$(load_pod)" -- sh -c 'tail -1 /acked/acked.log 2>/dev/null' | awk '{print $2}')
    now=$(epoch)
    [ -n "$last" ] && [ "$last" -gt "${T0:-0}" ] && [ $(( now - last )) -lt 5 ] && return 0
    sleep 5
  done
  return 1
}

pre_rwx() {
  need_env
  harness_healthy
  kubectl get pvc -n "$NS" data-pg-0 -o jsonpath='{.spec.accessModes[0]}' | grep -q ReadWriteMany \
    || fail "harness PVC is not RWX — redeploy: SC=flint MODE=RWX WITNESS=1 ./deploy-harness.sh reset"
  PRE_NODE=$(pg_node); PRE_UID=$(pg_uid); PRE_RESTARTS=$(pg_restarts)
  PV=$(pg_pv); NFS_POD=$(nfs_pod); NFS_NODE=$(nfs_node); NFS_UID=$(nfs_pod_uid)
  [ -n "$NFS_POD" ] || fail "no flint-nfs pod found for $PV"
  export PRE_NODE PRE_UID PRE_RESTARTS PV NFS_POD NFS_NODE NFS_UID
  T0=$(epoch)
  step "T0=$T0 pg-0 on $PRE_NODE; nfs $NFS_POD on $NFS_NODE (uid ${NFS_UID:0:8}); witness on $(kubectl get pod -n "$NS" -l app=witness -o jsonpath='{.items[0].spec.nodeName}' 2>/dev/null)"
}

verify() { ./verify-drill.sh "$PHASE_LABEL" "$DRILL" "$T0"; }

CORDONED=""; TAINTED=""; DEAD_IID=""
restore() {
  set +e
  [ -n "$CORDONED" ] && kubectl uncordon "$CORDONED" >/dev/null 2>&1
  [ -n "$TAINTED" ] && untaint_oos "$TAINTED"
  [ -n "$DEAD_IID" ] && kubelet_start_ssm "$DEAD_IID"
}
trap restore EXIT

case "$DRILL" in

3.1) # graceful cross-node migration — exactly one nfs pod, witness clean
  pre_rwx
  kubectl cordon "$PRE_NODE" >/dev/null; CORDONED="$PRE_NODE"
  kubectl delete pod -n "$NS" $PG --wait=false
  MAXPODS=1
  for i in $(seq 1 40); do
    c=$(nfs_pod_count); [ "${c:-1}" -gt "$MAXPODS" ] && MAXPODS=$c
    NEW_UID=$(kubectl get pod -n "$NS" $PG -o jsonpath='{.metadata.uid}' 2>/dev/null)
    RD=$(kubectl get pod -n "$NS" $PG -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}' 2>/dev/null)
    [ -n "$NEW_UID" ] && [ "$NEW_UID" != "$PRE_UID" ] && [ "$RD" = "True" ] && break
    sleep 5
  done
  kubectl uncordon "$PRE_NODE" >/dev/null; CORDONED=""
  [ "$MAXPODS" -eq 1 ] && ok "exactly one nfs pod throughout" || note "DUPLICATE nfs pods seen: max=$MAXPODS"
  [ "$(nfs_pod_uid)" = "$NFS_UID" ] && ok "nfs pod untouched (same uid)" || note "nfs pod was RECREATED during client migration"
  witness_verdict "$T0"
  EXPECT_RESCHEDULE=cross NOTES="RWX cross-node migration; nfs_pods_max=$MAXPODS nfs_uid_same=$([ "$(nfs_pod_uid)" = "$NFS_UID" ] && echo Y || echo N)" verify
  ;;

3.1b) # force delete + in-container SIGKILL — dirty postmaster over NFS
  pre_rwx
  kubectl exec -n "$NS" $PG -c chaos -- pkill -9 -x postgres 2>/dev/null || true
  kubectl delete pod -n "$NS" $PG --grace-period=0 --force --wait=false
  wait_pod_replaced "$NS" $PG "$PRE_UID" 300 || fail "replacement never Ready"
  witness_verdict "$T0"
  NOTES="RWX force delete + pkill (dirty postmaster over NFS); WAL replay expected" verify
  ;;

3.2) # nfs pod delete → liveness reconciler recreates
  pre_rwx
  kubectl delete pod -n "$DRIVER_NS" "$NFS_POD" --wait=false
  note "nfs pod deleted; waiting for reconciler recreate"
  T_REC=-1
  for i in $(seq 1 36); do
    U=$(nfs_pod_uid)
    if [ -n "$U" ] && [ "$U" != "$NFS_UID" ]; then
      PH=$(kubectl get pod -n "$DRIVER_NS" "$(nfs_pod)" -o jsonpath='{.status.phase}' 2>/dev/null)
      [ "$PH" = "Running" ] && { T_REC=$(( $(epoch) - T0 )); break; }
    fi
    sleep 5
  done
  [ "$T_REC" -ge 0 ] && ok "nfs pod recreated+Running at ${T_REC}s" || note "nfs pod NOT recreated in 180s"
  wait_acks_fresh 240 && T_RESUME=$(( $(epoch) - T0 )) || T_RESUME=-1
  ESTALE=$(kubectl logs -n "$NS" $PG -c postgres --since-time="$(rfc3339 "$T0")" 2>/dev/null | grep -ci "stale file" || true)
  [ "${ESTALE:-0}" -eq 0 ] && ok "no ESTALE on client" || note "ESTALE lines: $ESTALE"
  witness_verdict "$T0"
  # READY_TIMEOUT 300: the client's TCP reconnect to the recreated
  # server rides the dead-backend black-hole tail (~180-220s observed
  # on runy2 u12.4 with a CLEAN db verdict) — 120s flagged known-good
  # runs as attribution failures.
  EXPECT_RESCHEDULE=none READY_TIMEOUT=300 \
    NOTES="nfs pod delete: recreate=${T_REC}s io_resume=${T_RESUME}s estale=$ESTALE" verify
  ;;

3.3a) # spdk-tgt PROCESS kill on the nfs-server's node
  pre_rwx
  IID=$(instance_id_for_node "$NFS_NODE")
  [ -n "$IID" ] || fail "no instance id for $NFS_NODE"
  SPDK_PRE=$(spdk_restarts "$NFS_NODE")
  ssm_run "$IID" "pkill -9 -f /usr/local/bin/spdk_tgt" >/dev/null
  note "spdk-tgt SIGKILL on nfs node $NFS_NODE"
  for i in $(seq 1 24); do
    [ "$(spdk_restarts "$NFS_NODE")" != "$SPDK_PRE" ] && break; sleep 5
  done
  [ "$(spdk_restarts "$NFS_NODE")" != "$SPDK_PRE" ] || fail "spdk-tgt never restarted — kill failed?"
  wait_acks_fresh 300 && T_RESUME=$(( $(epoch) - T0 )) || T_RESUME=-1
  NFS_RESTARTS=$(kubectl get pod -n "$DRIVER_NS" "$(nfs_pod)" -o jsonpath='{.status.containerStatuses[0].restartCount}' 2>/dev/null)
  [ "$(nfs_pod_uid)" = "$NFS_UID" ] && ok "nfs pod NOT recreated" || note "nfs pod recreated"
  witness_verdict "$T0"
  EXPECT_RESCHEDULE=none READY_TIMEOUT=180 \
    NOTES="spdk-tgt kill on nfs node: io_resume=${T_RESUME}s nfs_restarts=$NFS_RESTARTS" verify
  ;;

3.3b) # csi-node POD delete on the nfs-server's node — F8 probe over NFS
  pre_rwx
  CNP=$(csi_node_pod "$NFS_NODE")
  kubectl delete pod -n "$DRIVER_NS" "$CNP" --wait=false
  note "csi-node POD on nfs node $NFS_NODE deleted (F8 probe)"
  kubectl wait --for=condition=Ready pod -l app=flint-csi-node -n "$DRIVER_NS" \
    --field-selector "spec.nodeName=$NFS_NODE" --timeout=180s >/dev/null 2>&1
  if wait_acks_fresh 300; then
    T_RESUME=$(( $(epoch) - T0 ))
    ok "I/O resumed ${T_RESUME}s — no F8 on the NFS path (record divergence)"
    EXPECT_RESCHEDULE=none READY_TIMEOUT=120 NOTES="nfs-node csi-node POD delete: SELF-RECOVERED io_resume=${T_RESUME}s" verify
  else
    note "I/O dead at 300s — F8 via nfs backing volume; recovery = nfs pod delete → reconciler"
    kubectl delete pod -n "$DRIVER_NS" "$(nfs_pod)" --wait=false 2>/dev/null
    wait_acks_fresh 420 || fail "I/O never resumed after nfs pod recreate"
    T_REC=$(( $(epoch) - T0 ))
    witness_verdict "$T0"
    READY_TIMEOUT=120 NOTES="nfs-node csi-node POD delete: F8 reproduced; recovery=nfs-pod recreate, total ${T_REC}s" verify
  fi
  ;;

3.4) # csi-node POD delete on the CLIENT node — no local block dependency
  pre_rwx
  CNP=$(csi_node_pod "$PRE_NODE")
  kubectl delete pod -n "$DRIVER_NS" "$CNP" --wait=false
  note "csi-node POD on client node $PRE_NODE deleted"
  WORST=0
  for i in $(seq 1 18); do
    last=$(timeout 15 kubectl exec -n "$NS" "$(load_pod)" -- sh -c 'tail -1 /acked/acked.log 2>/dev/null' | awk '{print $2}')
    age=$(( $(epoch) - ${last:-0} ))
    [ "$age" -gt "$WORST" ] && WORST=$age
    sleep 5
  done
  [ "$WORST" -le 10 ] && ok "no client stall (worst ack age ${WORST}s)" || note "client stall: worst ack age ${WORST}s"
  witness_verdict "$T0"
  EXPECT_RESCHEDULE=none READY_TIMEOUT=60 NOTES="client-node csi-node delete: worst_ack_age=${WORST}s" verify
  ;;

3.5) # controller kill mid-ControllerPublish of a fresh RWX attach
  pre_rwx
  kubectl cordon "$PRE_NODE" >/dev/null; CORDONED="$PRE_NODE"
  kubectl delete pod -n "$NS" $PG --wait=false
  sleep "${CTRL_KILL_DELAY:-4}"
  kubectl delete pod -n "$DRIVER_NS" "$(controller_pod)" --wait=false
  note "controller killed ${CTRL_KILL_DELAY:-4}s into RWX re-publish"
  MAXPODS=1
  for i in $(seq 1 60); do
    c=$(nfs_pod_count); [ "${c:-1}" -gt "$MAXPODS" ] && MAXPODS=$c
    NEW_UID=$(kubectl get pod -n "$NS" $PG -o jsonpath='{.metadata.uid}' 2>/dev/null)
    RD=$(kubectl get pod -n "$NS" $PG -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}' 2>/dev/null)
    [ -n "$NEW_UID" ] && [ "$NEW_UID" != "$PRE_UID" ] && [ "$RD" = "True" ] && break
    sleep 5
  done
  kubectl uncordon "$PRE_NODE" >/dev/null; CORDONED=""
  [ "$MAXPODS" -eq 1 ] && ok "no duplicate nfs pods through controller death" || note "DUPLICATE nfs pods: max=$MAXPODS"
  witness_verdict "$T0"
  EXPECT_RESCHEDULE=cross READY_TIMEOUT=500 NOTES="controller kill mid-RWX-publish; nfs_pods_max=$MAXPODS" verify
  ;;

3.6) # nfs-server NODE kill on an r2 volume — reconciler must resurrect on the
     # surviving replica node. Requires harness: SC=flint-r2 MODE=RWX WITNESS=1
  pre_rwx
  kubectl get pv "$PV" -o jsonpath='{.spec.storageClassName}' | grep -q "flint-r2" \
    || fail "3.6 needs SC=flint-r2 (current: $(kubectl get pv "$PV" -o jsonpath='{.spec.storageClassName}')) — reset the harness"
  IID=$(instance_id_for_node "$NFS_NODE")
  [ -n "$IID" ] || fail "no instance id for $NFS_NODE"
  kubelet_stop "$NFS_NODE"; DEAD_IID="$IID"
  wait_node_notready "$NFS_NODE" 180 || fail "nfs node never NotReady"
  taint_oos "$NFS_NODE"; TAINTED="$NFS_NODE"
  T_REC=-1
  for i in $(seq 1 72); do
    U=$(nfs_pod_uid); N=$(nfs_node)
    if [ -n "$U" ] && [ "$U" != "$NFS_UID" ] && [ -n "$N" ] && [ "$N" != "$NFS_NODE" ]; then
      PH=$(kubectl get pod -n "$DRIVER_NS" "$(nfs_pod)" -o jsonpath='{.status.phase}' 2>/dev/null)
      [ "$PH" = "Running" ] && { T_REC=$(( $(epoch) - T0 )); break; }
    fi
    sleep 5
  done
  [ "$T_REC" -ge 0 ] && ok "nfs pod resurrected on $(nfs_node) at ${T_REC}s" || note "nfs pod NOT resurrected in 360s"
  wait_acks_fresh 420 && T_RESUME=$(( $(epoch) - T0 )) || T_RESUME=-1
  witness_verdict "$T0"
  EXPECT_RESCHEDULE=none READY_TIMEOUT=180 \
    NOTES="nfs NODE kill (r2): resurrect=${T_REC}s on $(nfs_node), io_resume=${T_RESUME}s" verify
  untaint_oos "$NFS_NODE"; TAINTED=""
  kubelet_start_ssm "$IID"; DEAD_IID=""
  wait_node_ready "$NFS_NODE" 300 && ok "nfs node restored" || note "node not Ready — check kubelet"
  ;;

3.7) # client node kill — STS replace + NFS remount elsewhere
  pre_rwx
  # disk-follows-pod places the backing volume — and therefore the nfs
  # server — on the CLIENT's node, so this drill usually kills BOTH
  # roles and inherits the server-kill (F33) class on top of the client
  # replacement. Surface the co-location so the verdict reads right.
  COLOC=N
  [ "$NFS_NODE" = "$PRE_NODE" ] && { COLOC=Y; note "nfs server CO-LOCATED on client node — this is also a server-node kill (F33 exposure)"; }
  IID=$(instance_id_for_node "$PRE_NODE")
  [ -n "$IID" ] || fail "no instance id for $PRE_NODE"
  kubelet_stop "$PRE_NODE"; DEAD_IID="$IID"
  wait_node_notready "$PRE_NODE" 180 || fail "client node never NotReady"
  taint_oos "$PRE_NODE"; TAINTED="$PRE_NODE"
  wait_pod_replaced "$NS" $PG "$PRE_UID" 400 || fail "replacement never Ready"
  witness_verdict "$T0"
  EXPECT_RESCHEDULE=cross READY_TIMEOUT=60 NOTES="client node kill + taint (RWX remount); nfs_colocated=$COLOC" verify
  untaint_oos "$PRE_NODE"; TAINTED=""
  kubelet_start_ssm "$IID"; DEAD_IID=""
  wait_node_ready "$PRE_NODE" 300 && ok "client node restored" || note "node not Ready — check kubelet"
  ;;

3.8) # client churn ×10 — nfs pod must be untouched throughout
  pre_rwx
  for i in $(seq 1 10); do
    CUR_NODE=$(pg_node); CUR_UID=$(pg_uid); C0=$(epoch)
    if [ $(( i % 2 )) -eq 0 ]; then
      kubectl cordon "$CUR_NODE" >/dev/null; CORDONED="$CUR_NODE"
    fi
    kubectl delete pod -n "$NS" $PG --wait=false
    wait_pod_replaced "$NS" $PG "$CUR_UID" 300 || fail "cycle $i: replacement never Ready"
    [ -n "$CORDONED" ] && { kubectl uncordon "$CORDONED" >/dev/null; CORDONED=""; }
    note "cycle=$i secs=$(( $(epoch) - C0 )) node=$(pg_node)"
  done
  [ "$(nfs_pod_uid)" = "$NFS_UID" ] && ok "nfs pod survived all 10 cycles (same uid)" || note "nfs pod RECREATED during churn"
  witness_verdict "$T0"
  NOTES="RWX churn x10; nfs_uid_same=$([ "$(nfs_pod_uid)" = "$NFS_UID" ] && echo Y || echo N)" verify
  ;;

3.9) # ☠ full csi-node DS roll — documented-limit drill, run last
  pre_rwx
  kubectl rollout restart ds/flint-csi-node -n "$DRIVER_NS" >/dev/null
  kubectl rollout status ds/flint-csi-node -n "$DRIVER_NS" --timeout=600s >/dev/null 2>&1
  ok "DS rolled"
  if wait_acks_fresh 300; then
    witness_verdict "$T0"
    NOTES="RWX DS roll: I/O survived" verify
  else
    note "I/O dead post-roll — recovering: nfs pod recreate, then client bounce if needed"
    kubectl delete pod -n "$DRIVER_NS" "$(nfs_pod)" --wait=false 2>/dev/null
    if ! wait_acks_fresh 300; then
      RUID=$(pg_uid)
      kubectl delete pod -n "$NS" $PG --grace-period=0 --force --wait=false
      wait_pod_replaced "$NS" $PG "$RUID" 400 || fail "bounce recovery failed"
      wait_acks_fresh 300 || fail "I/O never resumed after nfs recreate + client bounce"
    fi
    READY_TIMEOUT=120 NOTES="RWX DS roll landmine; recovery=nfs recreate(+client bounce)" verify
  fi
  ;;

*) fail "unknown drill '$DRILL'" ;;
esac
