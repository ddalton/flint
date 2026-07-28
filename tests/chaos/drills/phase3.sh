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
#   3.6c  F36c gate TRANSIENT: degrade one leg (writer set shrinks), then
#         kill the server node while the FRESH leg's claims strand there —
#         the resurrect must DEFER (not serve the trailing leg); zero loss
#         once the fresh leg rejoins. Needs SC=flint-r2 + v1.19+ driver.
#   3.6d ☠ F36c gate PERMANENT: same setup but TERMINATE the server node
#         (fresh local leg dies with it) — must serve the trailing leg
#         within the defer bound + raise flint.io/acked-tail-risk.
#         EXPECTED-BOUNDED-LOSS drill: db verdict shows the trailed tail.
#   3.6e ☠ F43 ACCEPTANCE (r2-perm): TERMINATE a backing-LEG node that is
#         NOT the nfs-server's node. The server stays alive and writing, so
#         the replacement can only be admitted by CUTOVER — the cell every
#         other 3.6* variant structurally cannot reach. Pre-fix the standby
#         parks forever; post-fix it lands in_sync. Needs SC=flint-r2 +
#         v1.20.0 driver and a spare storage node (consumes one).
#   3.6f ☠ F49 ACCEPTANCE (server-onto-leg) + F47: MOVE the nfs server onto
#         the node hosting the OTHER leg (cordon the rest, delete the pod).
#         That node's legitimately-remote leg export becomes a consumer-local
#         SQUATTER holding the lvol, so pre-fix bdev_raid_create loops on
#         EPERM and the volume is unmountable. Also asserts the outgoing
#         server keeps zero loopback shells (F47). Needs SC=flint-r2 +
#         v1.21.0 driver. Consumes no nodes — repeatable.
#   3.7   client node kill (kubelet stop + taint)
#   3.8   client churn ×10 — nfs pod must survive untouched (same UID)
#   3.9 ☠ full csi-node DS roll (documented-limit drill, run last)
#   3.10  F37: force-delete the nfs pod ×3 (same-node recreate races
#         NodeUnstage) — assert ONE ublk id per bdev after each cycle,
#         acks stay fresh (no EIO), reap lines attributed in agent logs.
#   3.11  v1.21.0 RWX online expansion under writes (controller-driven
#         backing chain; backing PVC status is the fs-growth proof)
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
  if [ -z "$last" ]; then
    # Empty read = the exec timed out on a hung mount (or empty log).
    # Say THAT — computing an age from 0 prints a raw epoch and reads
    # like a bizarre-but-ignorable number (runz 3.6 postmortem).
    note "WITNESS: UNRESPONSIVE (mount read timed out — flows likely hung; mismatches=$mism)"
    return 1
  fi
  local age=$(( $(epoch) - last ))
  if [ "${mism:-0}" -eq 0 ] && [ "$age" -lt 15 ]; then
    ok "witness clean (0 mismatches, last write ${age}s ago)"; return 0
  fi
  note "WITNESS: mismatches=$mism last-write-age=${age}s"; return 1
}

wait_witness_fresh() { # [budget_s] — seconds until the witness writes fresh again, else -1.
  # THE F33 acceptance metric: a hung witness must recover once the
  # orphan's sockets die (fence F33b) — bounded so a dead witness can
  # never wedge the drill (the original 87-minute 3.6 hang).
  local budget=${1:-300} t0=$(epoch) wp last age
  while [ $(( $(epoch) - t0 )) -lt "$budget" ]; do
    wp=$(witness_pod)
    if [ -n "$wp" ]; then
      last=$(timeout 15 kubectl exec -n "$NS" "$wp" -- sh -c 'tail -1 /mnt/witness.log' 2>/dev/null | awk '{print $2}')
      if [ -n "$last" ]; then
        age=$(( $(epoch) - last ))
        [ "$age" -lt 15 ] && { echo $(( $(epoch) - T0 )); return 0; }
      fi
    fi
    sleep 10
  done
  echo -1; return 1
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

# ---- F36c / F37 observability helpers -----------------------------------
SYNC_ANNO='flint\.csi\.storage\.io/replica-sync-state'
sync_record()      { kubectl get pv "$PV" -o jsonpath="{.metadata.annotations.$SYNC_ANNO}" 2>/dev/null; }
writer_uuids()     { sync_record | jq -r '.writer_set.lvol_uuids[]?' 2>/dev/null; }
leg_state()        { sync_record | jq -r --arg u "$1" '.replicas[]? | select(.lvol_uuid==$u) | .sync_state' 2>/dev/null; }
pv_replicas_json() { kubectl get pv "$PV" -o json 2>/dev/null | jq -r '.spec.csi.volumeAttributes["flint.csi.storage.io/replicas"] // empty'; }
risk_annotation()  { kubectl get pv "$PV" -o jsonpath='{.metadata.annotations.flint\.io/acked-tail-risk}' 2>/dev/null; }

driver_log_hits() { # <t0> <pattern> — hits across every csi-node driver log
  local t total=0 n p c; t=$(rfc3339 "$1")
  for n in $(worker_nodes); do
    p=$(csi_node_pod "$n"); [ -n "$p" ] || continue
    c=$(kubectl logs -n "$DRIVER_NS" "$p" -c flint-csi-driver --since-time="$t" 2>/dev/null | grep -c "$2" || true)
    total=$(( total + c ))
  done
  echo "$total"
}

pv_events_since() { # <t0> <reason> — count of PV events since t0
  kubectl get events -n default --field-selector reason="$2" -o json 2>/dev/null \
    | jq -r --arg t "$(rfc3339 "$1")" --arg pv "$PV" \
      '[.items[] | select((.lastTimestamp // .eventTime // "1970") >= $t)
                 | select(.involvedObject.name == $pv or ((.message // "") | contains($pv)))] | length'
}

last_ack_line() { timeout 15 kubectl exec -n "$NS" "$(load_pod)" -- sh -c 'tail -1 /acked/acked.log 2>/dev/null'; }

degrade_remote_leg() { # picks the leg NOT on $NFS_NODE, kills its spdk-tgt,
  # and waits for the record to mark it stale + drop it from the writer set.
  # Exports DEG_NODE DEG_UUID FRESH_UUID GATE_ARMED.
  local repl; repl=$(pv_replicas_json)
  DEG_NODE=$(echo "$repl" | jq -r '.[].node_name' | grep -v "^$NFS_NODE$" | head -1)
  [ -n "$DEG_NODE" ] || fail "no remote leg to degrade (both legs on $NFS_NODE?)"
  DEG_UUID=$(echo "$repl" | jq -r --arg n "$DEG_NODE" '.[] | select(.node_name==$n) | .lvol_uuid' | head -1)
  FRESH_UUID=$(echo "$repl" | jq -r --arg n "$DEG_NODE" '.[] | select(.node_name!=$n) | .lvol_uuid' | head -1)
  local iid; iid=$(instance_id_for_node "$DEG_NODE")
  [ -n "$iid" ] || fail "no instance id for $DEG_NODE"
  ssm_run "$iid" "pkill -9 -f /usr/local/bin/spdk_tgt" >/dev/null
  note "spdk-tgt killed on $DEG_NODE (leg ${DEG_UUID:0:8}…) — raid degrades, writes continue on the fresh leg"
  GATE_ARMED=0
  local i st ws
  for i in $(seq 1 36); do
    st=$(leg_state "$DEG_UUID"); ws=$(writer_uuids | tr '\n' ' ')
    if [ "$st" != "in_sync" ] && [ -n "$ws" ] && ! echo "$ws" | grep -q "$DEG_UUID"; then
      GATE_ARMED=1; break
    fi
    sleep 5
  done
  if [ "$GATE_ARMED" = 1 ]; then
    ok "writer set shrunk to the fresh leg (deg leg state=$(leg_state "$DEG_UUID"))"
  else
    note "writer set never shrank — record: $(sync_record | jq -c '{ws: .writer_set, states: [.replicas[]? | {u: .lvol_uuid[0:8], s: .sync_state}]}' 2>/dev/null)"
  fi
  # Let the fresh leg accumulate a post-shrink acked tail — the delta the
  # gate exists to protect.
  sleep 20
  export DEG_NODE DEG_UUID FRESH_UUID GATE_ARMED
}

# ---- r2 leg helpers (3.6e) ----------------------------------------------
# Same shapes phase2.sh uses; duplicated rather than sourced because the two
# drivers are independently runnable.
spdk_rpc() { # <node> <rpc args...> — spdk RPC via the node's spdk-tgt container
  local pod; pod=$(csi_node_pod "$1"); shift
  [ -n "$pod" ] || return 1
  kubectl exec -n "$DRIVER_NS" "$pod" -c spdk-tgt -- sh -c \
    "rpc.py $* 2>/dev/null || /usr/local/scripts/rpc.py $* 2>/dev/null || python3 /usr/local/scripts/rpc.py $* 2>/dev/null"
}

raid_summary() { # <node> — compact raid bdev state on the node
  spdk_rpc "$1" bdev_raid_get_bdevs all 2>/dev/null \
    | jq -r '.[] | .name + " state=" + .state + " base=" + ((.base_bdevs_list // []) | map(select(.is_configured)) | length | tostring) + "/" + (.num_base_bdevs | tostring)' 2>/dev/null
}

evict_load_from() { # <node...> — the ledger oracle must survive the drill.
  # acked.log IS the loss ground truth; if it dies with the target node the
  # drill blinds itself (the 2u/2.3 lesson).
  local lp ln n hit=""
  lp=$(load_pod); [ -n "$lp" ] || return 0
  ln=$(kubectl get pod -n "$NS" "$lp" -o jsonpath='{.spec.nodeName}' 2>/dev/null)
  for n in "$@"; do [ "$ln" = "$n" ] && hit=1; done
  [ -n "$hit" ] || return 0
  note "ledger oracle on drill-target $ln — relocating (acked.log must survive)"
  for n in "$@"; do kubectl cordon "$n" >/dev/null 2>&1; done
  kubectl delete pod -n "$NS" "$lp" --wait=false
  kubectl wait --for=delete pod -n "$NS" "$lp" --timeout=120s >/dev/null 2>&1
  local i
  for i in $(seq 1 24); do
    lp=$(load_pod); [ -n "$lp" ] && break; sleep 5
  done
  for n in "$@"; do kubectl uncordon "$n" >/dev/null 2>&1; done
  [ -n "$lp" ] || fail "load pod never came back after relocation"
  ok "oracle relocated to $(kubectl get pod -n "$NS" "$lp" -o jsonpath='{.spec.nodeName}')"
}

leg_state_on() { # <node> — sync_state of this volume's replica on a node
  sync_record | jq -r --arg n "$1" '.replicas[]? | select(.node_name==$n) | .sync_state' 2>/dev/null | head -1
}

vol_subsystems_on() { # <node> <pv> — flint subsystem NQNs on the node naming this volume
  # Via the node-agent proxy, not rpc.py: the driver's node container has no
  # rpc.py (runag lesson). Covers BOTH id domains — a wrapper-named
  # (`nfs-server-<pv>`) subsystem still contains the pv substring.
  agent_spdk_rpc "$1" '{"method":"nvmf_get_subsystems"}' \
    | jq -r --arg pv "$2" '.result[]? | select(.nqn | contains($pv)) | .nqn' 2>/dev/null
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

CORDONED=""; TAINTED=""; DEAD_IID=""; CORDONED_MANY=""
restore() {
  set +e
  [ -n "$CORDONED" ] && kubectl uncordon "$CORDONED" >/dev/null 2>&1
  # 3.6f cordons the whole fleet bar one node to force placement; an early
  # exit must never leave the cluster unschedulable.
  for n in $CORDONED_MANY; do kubectl uncordon "$n" >/dev/null 2>&1; done
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
  # F33 acceptance: the witness must resume WITHOUT manual unsticking,
  # within ~fence-deadline + reconnect. Measured before the ack wait so
  # the metric is the witness's own recovery, not the harness's.
  T_WITNESS=$(wait_witness_fresh 300)
  [ "$T_WITNESS" -ge 0 ] && ok "witness recovered at ${T_WITNESS}s (F33 acceptance)" \
    || note "witness NOT recovered in 300s — F33 fence did not release clients"
  wait_acks_fresh 420 && T_RESUME=$(( $(epoch) - T0 )) || T_RESUME=-1
  witness_verdict "$T0"
  EXPECT_RESCHEDULE=none READY_TIMEOUT=180 \
    NOTES="nfs NODE kill (r2): resurrect=${T_REC}s on $(nfs_node), witness_recovery=${T_WITNESS}s, io_resume=${T_RESUME}s" verify
  [ "$T_WITNESS" -ge 0 ] || fail "3.6 FAIL: witness never recovered (F33/F33b)"
  untaint_oos "$NFS_NODE"; TAINTED=""
  kubelet_start_ssm "$IID"; DEAD_IID=""
  wait_node_ready "$NFS_NODE" 300 && ok "nfs node restored" || note "node not Ready — check kubelet"
  ;;

3.6c) # F36c gate TRANSIENT — the run-3 shape, deterministic: shrink the
      # writer set to the fresh leg, then kill the server node so the fresh
      # leg's claims strand there. The resurrect must DEFER on the missing
      # writer (never serve the trailing leg) until guard-b clears the
      # claim / the node returns; the db verdict must be ZERO loss.
  pre_rwx
  kubectl get pv "$PV" -o jsonpath='{.spec.storageClassName}' | grep -q "flint-r2" \
    || fail "3.6c needs SC=flint-r2 — reset the harness"
  [ -n "$(writer_uuids)" ] || fail "no writer_set on the sync record — driver predates F36c (need v1.19+)"
  degrade_remote_leg
  ACK_AT_KILL=$(last_ack_line)
  note "acked tail at kill: $ACK_AT_KILL"
  IID=$(instance_id_for_node "$NFS_NODE")
  [ -n "$IID" ] || fail "no instance id for $NFS_NODE"
  kubelet_stop "$NFS_NODE"; DEAD_IID="$IID"
  wait_node_notready "$NFS_NODE" 180 || fail "server node never NotReady"
  taint_oos "$NFS_NODE"; TAINTED="$NFS_NODE"
  # Resurrect budget = 3.6's 360s + the gate's defer bound (180s).
  T_REC=-1
  for i in $(seq 1 108); do
    U=$(nfs_pod_uid); N=$(nfs_node)
    if [ -n "$U" ] && [ "$U" != "$NFS_UID" ] && [ -n "$N" ] && [ "$N" != "$NFS_NODE" ]; then
      PH=$(kubectl get pod -n "$DRIVER_NS" "$(nfs_pod)" -o jsonpath='{.status.phase}' 2>/dev/null)
      [ "$PH" = "Running" ] && { T_REC=$(( $(epoch) - T0 )); break; }
    fi
    sleep 5
  done
  [ "$T_REC" -ge 0 ] && ok "nfs pod resurrected on $(nfs_node) at ${T_REC}s" || note "nfs pod NOT resurrected in 540s"
  T_WITNESS=$(wait_witness_fresh 420)
  [ "$T_WITNESS" -ge 0 ] && ok "witness recovered at ${T_WITNESS}s" || note "witness NOT recovered in 420s"
  wait_acks_fresh 420 && T_RESUME=$(( $(epoch) - T0 )) || T_RESUME=-1
  # Gate observability: defers seen, and the trailing leg never admitted.
  DEFERS=$(driver_log_hits "$T0" "F36C DEFER")
  DEFER_EV=$(pv_events_since "$T0" AssemblyDeferred)
  STALE_ADMIT=$(pv_events_since "$T0" StaleReplicaAdmitted)
  RISK=$(risk_annotation)
  [ "$STALE_ADMIT" = "0" ] && ok "trailing leg never force-admitted (StaleReplicaAdmitted=0)" \
    || note "GATE BYPASSED? StaleReplicaAdmitted events: $STALE_ADMIT"
  [ "${DEFERS:-0}" -gt 0 ] || [ "${DEFER_EV:-0}" -gt 0 ] \
    && note "gate deferred assembly (log_hits=$DEFERS events=$DEFER_EV)" \
    || note "no defer observed — fresh leg attached first try (gate pass-through; weaker but valid)"
  [ -z "$RISK" ] && ok "no acked-tail-risk raised (transient branch held)" \
    || note "UNEXPECTED acked-tail-risk on transient drill: $RISK"
  witness_verdict "$T0"
  EXPECT_RESCHEDULE=none READY_TIMEOUT=180 \
    NOTES="F36c TRANSIENT: gate_armed=$GATE_ARMED defers=$DEFERS/$DEFER_EV stale_admit=$STALE_ADMIT resurrect=${T_REC}s witness=${T_WITNESS}s io_resume=${T_RESUME}s risk=${RISK:-none}" verify
  [ "$STALE_ADMIT" = "0" ] || fail "3.6c FAIL: trailing leg was admitted while the writer leg was transiently unavailable"
  untaint_oos "$NFS_NODE"; TAINTED=""
  kubelet_start_ssm "$IID"; DEAD_IID=""
  wait_node_ready "$NFS_NODE" 300 && ok "server node restored" || note "node not Ready — check kubelet"
  ;;

3.6d) # ☠ F36c gate PERMANENT — terminate the server node so the fresh
      # (co-located) leg dies with it. The gate must NOT hang: serve the
      # trailing leg within the defer bound and surface
      # flint.io/acked-tail-risk + AckedTailRisk. EXPECTED-BOUNDED-LOSS:
      # the db verdict SHOWS the post-shrink tail; the assertion is that
      # the loss is surfaced and bounded, not that it is zero.
  pre_rwx
  kubectl get pv "$PV" -o jsonpath='{.spec.storageClassName}' | grep -q "flint-r2" \
    || fail "3.6d needs SC=flint-r2 — reset the harness"
  [ -n "$(writer_uuids)" ] || fail "no writer_set on the sync record — driver predates F36c (need v1.19+)"
  degrade_remote_leg
  ACK_AT_KILL=$(last_ack_line)
  note "acked tail at kill (upper bound of the expected loss): $ACK_AT_KILL"
  IID=$(instance_id_for_node "$NFS_NODE")
  [ -n "$IID" ] || fail "no instance id for $NFS_NODE"
  aws ec2 terminate-instances --region "$AWS_REGION" --instance-ids "$IID" >/dev/null
  note "TERMINATED $IID ($NFS_NODE) — fresh local leg is GONE; trailing leg must be served WITH the risk surfaced"
  wait_node_notready "$NFS_NODE" 300
  taint_oos "$NFS_NODE"; TAINTED="$NFS_NODE"
  T_REC=-1
  for i in $(seq 1 144); do
    U=$(nfs_pod_uid); N=$(nfs_node)
    if [ -n "$U" ] && [ "$U" != "$NFS_UID" ] && [ -n "$N" ] && [ "$N" != "$NFS_NODE" ]; then
      PH=$(kubectl get pod -n "$DRIVER_NS" "$(nfs_pod)" -o jsonpath='{.status.phase}' 2>/dev/null)
      [ "$PH" = "Running" ] && { T_REC=$(( $(epoch) - T0 )); break; }
    fi
    sleep 5
  done
  [ "$T_REC" -ge 0 ] && ok "nfs pod resurrected on $(nfs_node) at ${T_REC}s (bound: defer 180s + reschedule)" \
    || note "nfs pod NOT resurrected in 720s — gate hung on a permanent loss? (2.4 REGRESSION)"
  T_WITNESS=$(wait_witness_fresh 420)
  wait_acks_fresh 420 && T_RESUME=$(( $(epoch) - T0 )) || T_RESUME=-1
  RISK=$(risk_annotation)
  RISK_EV=$(pv_events_since "$T0" AckedTailRisk)
  [ -n "$RISK" ] && ok "acked-tail-risk surfaced: $RISK" || note "MISSING flint.io/acked-tail-risk annotation"
  [ "${RISK_EV:-0}" -gt 0 ] && ok "AckedTailRisk event raised" || note "MISSING AckedTailRisk event"
  witness_verdict "$T0"
  EXPECT_RESCHEDULE=none READY_TIMEOUT=180 \
    NOTES="F36c PERMANENT (EXPECTED-BOUNDED-LOSS): gate_armed=$GATE_ARMED resurrect=${T_REC}s witness=${T_WITNESS}s io_resume=${T_RESUME}s risk_anno=$([ -n "$RISK" ] && echo Y || echo N) risk_ev=$RISK_EV ack_at_kill='$ACK_AT_KILL'" verify
  [ "$T_REC" -ge 0 ] || fail "3.6d FAIL: no resurrect — the gate manufactured an outage on permanent loss"
  [ -n "$RISK" ] || fail "3.6d FAIL: loss not surfaced (no acked-tail-risk annotation)"
  untaint_oos "$NFS_NODE"; TAINTED=""
  kubectl delete node "$NFS_NODE" >/dev/null 2>&1
  note "NEXT: node terminated — cluster is a worker down; volume single-leg until re-placement"
  ;;

3.6e) # ☠ F43 ACCEPTANCE (r2-perm) — permanently TERMINATE a backing-LEG node
      # that is NOT the nfs-server's node, then delete its Node object.
      #
      # This is the cell the campaign never ran. Every prior 3.6* variant
      # kills the NFS SERVER's node, which structurally cannot show the bug:
      # the server dies, gets resurrected elsewhere, and the fresh stage
      # re-admits legs on the attach path. Killing a REMOTE leg leaves the
      # server alive and writing, so the replacement can only be admitted by
      # CUTOVER — the resolver that owns RWX admission via the NFS bounce.
      #
      # F43: cutover is perpetually out-raced for the per-volume claim by
      # catch-up, whose epoch scheduler re-claims on a 30s writes-independent
      # timer. Pre-fix the replacement converges and then PARKS at
      # sync_state=standby forever; the volume never returns to 2/2.
      # Post-fix (v1.20.0 #1) cutover RESERVES the next claim and lands
      # within ~2 maintainer ticks.
  pre_rwx
  kubectl get pv "$PV" -o jsonpath='{.spec.storageClassName}' | grep -q "flint-r2" \
    || fail "3.6e needs SC=flint-r2 (current: $(kubectl get pv "$PV" -o jsonpath='{.spec.storageClassName}')) — reset the harness"
  REPL=$(pv_replicas_json)
  LEG_NODE=$(echo "$REPL" | jq -r '.[].node_name' | grep -v "^$NFS_NODE$" | head -1)
  [ -n "$LEG_NODE" ] || fail "both legs live on the nfs node ($NFS_NODE) — 3.6e needs a REMOTE leg"
  DEAD_UUID=$(echo "$REPL" | jq -r --arg n "$LEG_NODE" '.[] | select(.node_name==$n) | .lvol_uuid' | head -1)
  [ "$LEG_NODE" != "$PRE_NODE" ] \
    || note "CAVEAT: the leg node is also the pg client node — client loss is conflated with storage loss"
  evict_load_from "$LEG_NODE"
  OVR_PRE=$(kubectl get pv "$PV" -o jsonpath='{.metadata.annotations.flint\.csi\.storage\.io/replicas-override}' 2>/dev/null)
  ACK_AT_KILL=$(last_ack_line)
  note "acked tail at kill: $ACK_AT_KILL"
  note "raid pre (on $NFS_NODE): $(raid_summary "$NFS_NODE" | head -2)"
  IID=$(instance_id_for_node "$LEG_NODE")
  [ -n "$IID" ] || fail "no instance id for $LEG_NODE"
  aws ec2 terminate-instances --region "$AWS_REGION" --instance-ids "$IID" >/dev/null
  note "TERMINATED $IID ($LEG_NODE) — remote leg ${DEAD_UUID:0:8}… permanently lost; the nfs server keeps writing"
  # P4 decomposition: timestamp the moment the raid actually deconfigures
  # the dead base (base=2/2 → 1/2 on the server node). Pre-fix this took
  # 116-176s (TCP blackhole: no RST → no transport error → fast_io_fail
  # never armed); with the dead-target timeouts it must land ≈ 8s (TCP
  # user-timeout) + fast_io_fail (20s). The stall's release ≈ this moment.
  DEGRADE_F=$(mktemp)
  ( while :; do
      B=$(raid_summary "$NFS_NODE" 2>/dev/null | grep -o 'base=[0-9]*/[0-9]*' | head -1)
      if [ -n "$B" ] && [ "$B" != "base=2/2" ]; then echo $(( $(epoch) - T0 )) > "$DEGRADE_F"; exit 0; fi
      [ $(( $(epoch) - T0 )) -ge 400 ] && exit 0
      sleep 5
    done ) &
  DEGRADE_PID=$!
  wait_node_notready "$LEG_NODE" 300
  kubectl delete node "$LEG_NODE" >/dev/null 2>&1
  T_NODEGONE=$(( $(epoch) - T0 ))
  note "Node object deleted at ${T_NODEGONE}s — re-placement trigger armed"

  # (a) F42 regression check: the raid must FAULT the dead leg, not stall.
  # Report the MEASURED gap, not just "recovered inside the budget". runai
  # 2026-07-27 printed "I/O never stalled" off a bare wait_acks_fresh 180
  # while writes were actually gone for ~150s (last ack T0+6s, controller
  # still state=resetting at T0+148s) — detection, not fast_io_fail, is what
  # dominates on the RWX path. runad's RWO 2.5 never paused at all, so the
  # two are not comparable and the message must not imply they are.
  if wait_acks_fresh 180; then
    KILL_GAP=$(max_stall_since "$T0")
    kill "$DEGRADE_PID" 2>/dev/null; wait "$DEGRADE_PID" 2>/dev/null
    T_DEGRADE=$(cat "$DEGRADE_F" 2>/dev/null); rm -f "$DEGRADE_F"
    [ -n "$T_DEGRADE" ] && note "raid deconfigured the dead base at ${T_DEGRADE}s (P4 detection decomposition)" \
      || { T_DEGRADE=-1; note "degrade moment not observed by the 5s watcher (raid gone/renamed before a poll saw 1/2?)"; }
    # P4 gate (post dead-target-timeouts): the whole leg-loss blindness —
    # blackhole detect + fast_io_fail + raid fault — must fit the budget.
    P4_BUDGET=${P4_STALL_BUDGET:-60}
    if [ "${KILL_GAP:-0}" -le 30 ]; then
      ok "I/O never stalled through the kill (max gap ${KILL_GAP}s — F42 fast_io_fail held)"
    elif [ "${KILL_GAP:-0}" -le "$P4_BUDGET" ]; then
      ok "P4 budget MET: stalled ${KILL_GAP}s ≤ ${P4_BUDGET}s (degrade at ${T_DEGRADE}s)"
    else
      note "P4 BUDGET EXCEEDED: stalled ${KILL_GAP}s > ${P4_BUDGET}s (degrade at ${T_DEGRADE}s) — detection latency still dominates"
    fi
  else
    kill "$DEGRADE_PID" 2>/dev/null; wait "$DEGRADE_PID" 2>/dev/null
    T_DEGRADE=$(cat "$DEGRADE_F" 2>/dev/null || echo -1); rm -f "$DEGRADE_F"
    note "acks went stale after the kill and never recovered in 180s — F42 REGRESSION?"
  fi

  # (b) F40: RWX re-placement must actually dispatch (identity swap).
  T_SWAP=-1; NEW_NODE=""
  for i in $(seq 1 60); do
    OVR=$(kubectl get pv "$PV" -o jsonpath='{.metadata.annotations.flint\.csi\.storage\.io/replicas-override}' 2>/dev/null)
    if [ -n "$OVR" ] && [ "$OVR" != "$OVR_PRE" ]; then
      T_SWAP=$(( $(epoch) - T0 ))
      NEW_NODE=$(echo "$OVR" | jq -r '.[].node_name' | grep -v "^$NFS_NODE$" | head -1)
      break
    fi
    sleep 10
  done
  [ "$T_SWAP" -ge 0 ] || fail "3.6e FAIL: replicas-override never appeared — RWX re-placement never dispatched (F40 regression)"
  ok "identity swapped to ${NEW_NODE:-?} at ${T_SWAP}s"

  # (c) ☠ THE F43 GATE — one convergence loop watching the replacement's
  # sync_state and the nfs pod uid together. Pre-fix: state parks at
  # "standby" and the uid never changes. Post-fix: cutover bounces the pod
  # and the leg lands in_sync.
  T_STANDBY=-1; T_CUTOVER=-1; T_SYNC=-1; ST=""; NFS_UID_NEW=""
  for i in $(seq 1 180); do        # 30 min budget
    ST=$(leg_state_on "$NEW_NODE")
    if [ "$T_STANDBY" -lt 0 ] && [ "$ST" = "standby" ]; then
      T_STANDBY=$(( $(epoch) - T0 )); note "replacement reached standby at ${T_STANDBY}s"
    fi
    U=$(nfs_pod_uid)
    if [ "$T_CUTOVER" -lt 0 ] && [ -n "$U" ] && [ "$U" != "$NFS_UID" ]; then
      T_CUTOVER=$(( $(epoch) - T0 )); NFS_UID_NEW=$U
      note "nfs pod BOUNCED at ${T_CUTOVER}s (uid ${NFS_UID:0:8} → ${U:0:8}) — cutover took the claim"
    fi
    [ "$ST" = "in_sync" ] && { T_SYNC=$(( $(epoch) - T0 )); break; }
    sleep 10
  done
  [ "$T_SYNC" -ge 0 ] && ok "replacement leg in_sync at ${T_SYNC}s — RWX redundancy restored (2/2)" \
    || note "replacement leg NOT in_sync after 30min (state=${ST:-unknown})"

  # (d) F44 settle assertion — the first run collapsed ~3min AFTER in_sync
  # (leaked leg controller on the outgoing server node → F36 head-in-use →
  # stale-mark → DegradedDirectServe). in_sync at an instant is not the
  # verdict; in_sync that SURVIVES the cutover cooldown is.
  T_SETTLED=-1; HEADINUSE=0; NFS_PHASE=""; NFS_READY=""; ASM_BLOCKED=""
  if [ "$T_SYNC" -ge 0 ]; then
    note "settle window: re-asserting 2/2 at +360s (F44 signature: ReplicaHeadInUse + collapse)"
    sleep 360
    N_SYNC=$(sync_record | jq -r '[.replicas[]?|select(.sync_state=="in_sync")]|length' 2>/dev/null)
    WS_N=$(writer_uuids | grep -c .)
    HEADINUSE=$(pv_events_since "$T0" ReplicaHeadInUse)
    # F46 lesson: record state is NOT liveness — run 3's settle "passed" on
    # a 2/2 record while the nfs pod sat Pending, unmountable. The settle
    # verdict now requires the SERVER to be alive too: pod Running+Ready
    # and no assembly-blocked marker on the PV.
    NFS_PHASE=$(kubectl get pod -n "$DRIVER_NS" "$(nfs_pod)" -o jsonpath='{.status.phase}' 2>/dev/null)
    NFS_READY=$(kubectl get pod -n "$DRIVER_NS" "$(nfs_pod)" \
      -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}' 2>/dev/null)
    ASM_BLOCKED=$(kubectl get pv "$PV" -o jsonpath='{.metadata.annotations.flint\.io/assembly-blocked}' 2>/dev/null)
    if [ "$N_SYNC" = "2" ] && [ "$NFS_PHASE" = "Running" ] && [ "$NFS_READY" = "True" ] && [ -z "$ASM_BLOCKED" ]; then
      T_SETTLED=$(( $(epoch) - T0 ))
      ok "redundancy HELD through the settle window (2/2 in_sync, writer_set=$WS_N, head_in_use_events=$HEADINUSE, nfs Running/Ready, no assembly-blocked marker)"
    elif [ "$N_SYNC" = "2" ]; then
      note "RECORD-ONLY health in the settle window: 2/2 in_sync but nfs phase=$NFS_PHASE ready=$NFS_READY assembly_blocked='${ASM_BLOCKED:-}' (F46 shape)"
    else
      note "COLLAPSED in the settle window: in_sync=$N_SYNC writer_set=$WS_N head_in_use_events=$HEADINUSE (F44 shape)"
    fi
  fi

  # (e) F44-cousin latent-pin sweep — a leaked copy/leg controller on any
  # node OTHER than the current server pins the head for future rebuilds
  # even while 2/2 serves fine (chase-controller leak, found run 2). This
  # only bites LATER, so assert it here rather than wait for the next drill.
  CUR_SRV=$(nfs_node)
  FOREIGN=""
  for n in $(worker_nodes); do
    [ "$n" = "$CUR_SRV" ] && continue
    C=$(spdk_rpc "$n" bdev_nvme_get_controllers 2>/dev/null | jq -r '[.[]?.name] | join(",")' 2>/dev/null | grep -o "${PV}" | head -1)
    [ -n "$C" ] && FOREIGN="$FOREIGN $n"
  done
  if [ -z "$FOREIGN" ]; then
    ok "no foreign leg/copy controllers linger off the server node (latent-pin sweep clean)"
  else
    note "LATENT PIN: volume controllers still attached on non-server node(s):$FOREIGN"
  fi

  CUT_START=$(pv_events_since "$T0" CutoverStarted)
  CUT_OK=$(pv_events_since "$T0" CutoverSucceeded)
  CUT_INEFF=$(pv_events_since "$T0" CutoverIneffective)
  YIELDS=$(driver_log_hits "$T0" "resolver reserved the next claim")
  SEIZES=$(driver_log_hits "$T0" "claim SEIZED")
  note "cutover events: started=$CUT_START succeeded=$CUT_OK ineffective=$CUT_INEFF"
  note "claim arbitration: maintainer yields=$YIELDS lease seizures=$SEIZES (seizures should be 0 — the lease is a wedge backstop, not the mechanism)"
  note "raid post (on $(nfs_node)): $(raid_summary "$(nfs_node)" | head -2)"

  T_WITNESS=$(wait_witness_fresh 420)
  [ "$T_WITNESS" -ge 0 ] && ok "witness fresh at ${T_WITNESS}s (multi-node access intact)" \
    || note "witness NOT fresh in 420s"
  wait_acks_fresh 300 && T_RESUME=$(( $(epoch) - T0 )) || T_RESUME=-1
  witness_verdict "$T0"
  RISK=$(risk_annotation)
  [ -z "$RISK" ] && ok "no acked-tail-risk raised (the surviving leg never trailed)" \
    || note "acked-tail-risk: $RISK"
  EXPECT_RESCHEDULE=none READY_TIMEOUT=180 \
    NOTES="F43 r2-perm: node_gone=${T_NODEGONE}s degrade=${T_DEGRADE:--1}s swap=${T_SWAP}s standby=${T_STANDBY}s cutover=${T_CUTOVER}s in_sync=${T_SYNC}s settled=${T_SETTLED}s witness=${T_WITNESS}s io_resume=${T_RESUME}s new_node=${NEW_NODE:-?} cut_ev=$CUT_START/$CUT_OK/$CUT_INEFF yields=$YIELDS seizes=$SEIZES head_in_use=$HEADINUSE risk=${RISK:-none}" verify

  # The F43 verdict. in_sync is the load-bearing assertion: a parked standby
  # is exactly the bug. The bounce is the expected mechanism — if redundancy
  # came back WITHOUT one, say so loudly rather than quietly passing.
  [ "$T_SYNC" -ge 0 ] \
    || fail "3.6e FAIL (F43): replacement parked at '${ST:-unknown}' — RWX admission is still starved; cutover never landed"
  [ "$T_SETTLED" -ge 0 ] \
    || fail "3.6e FAIL (F44/F46): the settle window did not hold — in_sync=${N_SYNC:-?}/2, nfs phase=${NFS_PHASE:-?} ready=${NFS_READY:-?}, assembly_blocked='${ASM_BLOCKED:-}' (record-only health = F46 shape; collapse = F44 shape, head_in_use_events=$HEADINUSE)"
  [ -z "$FOREIGN" ] \
    || fail "3.6e FAIL (F44-cousin): latent head pin — foreign controllers on$FOREIGN would deadlock the NEXT rebuild (chase-controller leak)"
  [ "$T_CUTOVER" -ge 0 ] \
    || note "3.6e ANOMALY: redundancy restored WITHOUT an nfs bounce — admitted by another path; confirm which before crediting the F43 fix"
  note "fleet is one storage node down — replace via trove before further node-consuming drills"
  ;;

3.6f) # ☠ F49 ACCEPTANCE (server-onto-leg) + F47 (unstage leaves no shell).
      # MOVE the NFS server onto the node that hosts the OTHER leg, then
      # assert the raid assembles there.
      #
      # Why a move and not a bounce in place: while the server runs on node
      # X, leg A on node A is consumed REMOTELY, so A legitimately carries a
      # leg export (`:volume:<pv>_<i>`, fenced to X). The moment the server
      # lands on A that export becomes a SQUATTER holding a write-mode open
      # on the lvol, and the raid module's exclusive claim fails EPERM.
      # NodeStage drops it (drop_stale_local_exports, correct NQN) — but
      # pre-fix two reconcilers re-mint it faster than the claim can land:
      # the 60s replica reconcile (no "consumer is me" skip) and the 10s
      # loss-detector (whose registry seed adopted leg NQNs). The volume
      # then loops `bdev_raid_create … Operation not permitted` forever and
      # the server is UNMOUNTABLE — an availability outage, no self-heal
      # (docs/f49-local-leg-export-squatter.md).
      #
      # F47 rides along: the OUTGOING server's unstage must leave ZERO
      # loopback subsystems for this volume — pre-fix the teardown belt
      # derived the WRAPPER NQN (silent no-op) and the F9 guard refused the
      # node its own loopback, leaving an empty bare-inner shell per cutover
      # (docs/f47-loopback-export-teardown-domain.md).
  pre_rwx
  kubectl get pv "$PV" -o jsonpath='{.spec.storageClassName}' | grep -q "flint-r2" \
    || fail "3.6f needs SC=flint-r2 (current: $(kubectl get pv "$PV" -o jsonpath='{.spec.storageClassName}')) — reset the harness"
  HANDLE=$(kubectl get pv "$PV" -o jsonpath='{.spec.csi.volumeHandle}')
  REPL=$(pv_replicas_json)
  # The target: a leg node that is NOT today's server. Landing there turns a
  # legitimately-remote leg export into a consumer-local squatter.
  TARGET=$(echo "$REPL" | jq -r '.[].node_name' | grep -v "^$NFS_NODE$" | head -1)
  [ -n "$TARGET" ] || fail "both legs already live on the server node ($NFS_NODE) — 3.6f needs a remote leg to move onto"
  TGT_IDX=$(echo "$REPL" | jq -r --arg n "$TARGET" 'to_entries[] | select(.value.node_name==$n) | .key' | head -1)
  LEG_NQN="nqn.2024-11.com.flint:volume:${PV}_${TGT_IDX}"
  OLD_SRV="$NFS_NODE"
  note "moving the nfs server $OLD_SRV → $TARGET (hosts leg #$TGT_IDX); squatter-to-be: $LEG_NQN"
  note "leg export on $TARGET pre-move: $(vol_subsystems_on "$TARGET" "$PV" | tr '\n' ' ')"
  note "raid pre (on $OLD_SRV): $(raid_summary "$OLD_SRV" | head -1)"
  ACK_AT_MOVE=$(last_ack_line)

  # Force placement: the pod's affinity to replica nodes is PREFERRED, so
  # cordoning the rest is what makes the landing deterministic.
  UNCORDON=""
  for n in $(worker_nodes); do
    [ "$n" = "$TARGET" ] && continue
    kubectl cordon "$n" >/dev/null 2>&1 && UNCORDON="$UNCORDON $n"
  done
  CORDONED_MANY="$UNCORDON"
  kubectl delete pod -n "$DRIVER_NS" "$NFS_POD" --wait=false >/dev/null 2>&1
  kubectl wait --for=delete pod -n "$DRIVER_NS" "$NFS_POD" --timeout=180s >/dev/null 2>&1

  # (a) THE F49 GATE: the server must come back Running+Ready ON the target.
  # Pre-fix this never happens — kubelet reports FailedMount while NodeStage
  # loops on EPERM.
  T_SERVED=-1; NEWPOD=""; NEWNODE=""
  for i in $(seq 1 60); do   # 60 x 10s = 10 min
    NEWPOD=$(nfs_pod)
    if [ -n "$NEWPOD" ]; then
      NEWNODE=$(kubectl get pod -n "$DRIVER_NS" "$NEWPOD" -o jsonpath='{.spec.nodeName}' 2>/dev/null)
      PH=$(kubectl get pod -n "$DRIVER_NS" "$NEWPOD" -o jsonpath='{.status.phase}' 2>/dev/null)
      RD=$(kubectl get pod -n "$DRIVER_NS" "$NEWPOD" \
        -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}' 2>/dev/null)
      if [ "$PH" = "Running" ] && [ "$RD" = "True" ]; then
        T_SERVED=$(( $(epoch) - T0 )); break
      fi
    fi
    sleep 10
  done
  for n in $CORDONED_MANY; do kubectl uncordon "$n" >/dev/null 2>&1; done
  CORDONED_MANY=""
  [ "$T_SERVED" -ge 0 ] \
    && ok "nfs server Running/Ready on $NEWNODE at ${T_SERVED}s (assembly succeeded on a leg-hosting node)" \
    || note "nfs server NEVER became Ready (pod=${NEWPOD:-none} node=${NEWNODE:-?}) — the F49 shape"

  # (b) the EPERM signature itself — the pre-fix loop, in the driver's words.
  EPERM_HITS=$(driver_log_hits "$T0" "Operation not permitted")
  RAIDFAIL=$(driver_log_hits "$T0" "bdev_raid_create failed")
  [ "$EPERM_HITS" = "0" ] && [ "$RAIDFAIL" = "0" ] \
    && ok "no bdev_raid_create EPERM anywhere in the fleet since T0" \
    || note "EPERM signature present: 'Operation not permitted'=$EPERM_HITS 'bdev_raid_create failed'=$RAIDFAIL"

  # (c) the FIX's own invariant: a consumer-local leg carries NO export.
  # (The raid consumes it as a raw bdev; an export could only squat.)
  LOCAL_LEG_EXPORT=$(vol_subsystems_on "$NEWNODE" "$PV" | grep -x "$LEG_NQN" || true)
  [ -z "$LOCAL_LEG_EXPORT" ] \
    && ok "consumer-local leg #$TGT_IDX carries no export on $NEWNODE (F49 fix 1 held)" \
    || note "SQUATTER PRESENT: $LOCAL_LEG_EXPORT still exported on the server's own node"

  # (d) the raid actually assembled 2/2 there.
  RAID_POST=$(raid_summary "$NEWNODE" | head -1)
  echo "$RAID_POST" | grep -q "base=2/2" \
    && ok "raid online 2/2 on $NEWNODE ($RAID_POST)" \
    || note "raid not 2/2 on $NEWNODE: ${RAID_POST:-<none>}"

  # (e) the OTHER leg must still be exported (fenced to the new server) —
  # the fix must not over-reach and tear down legitimately-remote exports.
  OTHER=$(echo "$REPL" | jq -r --arg n "$TARGET" '.[] | select(.node_name!=$n) | .node_name' | head -1)
  if [ -n "$OTHER" ] && [ "$OTHER" != "$NEWNODE" ]; then
    OTHER_IDX=$(echo "$REPL" | jq -r --arg n "$OTHER" 'to_entries[] | select(.value.node_name==$n) | .key' | head -1)
    OTHER_NQN="nqn.2024-11.com.flint:volume:${PV}_${OTHER_IDX}"
    vol_subsystems_on "$OTHER" "$PV" | grep -qx "$OTHER_NQN" \
      && ok "remote leg #$OTHER_IDX still exported on $OTHER (no over-reach)" \
      || note "OVER-REACH: the remote leg export $OTHER_NQN is gone from $OTHER — the raid cannot reach that leg"
  fi

  # (f) F47: the outgoing server must retain ZERO loopback subsystems.
  # Leg exports on that node are legitimate and excluded by shape.
  OLD_SHELLS=$(vol_subsystems_on "$OLD_SRV" "$PV" \
    | grep -E "^nqn\.2024-11\.com\.flint:volume:(nfs-server-)?${PV}$" || true)
  [ -z "$OLD_SHELLS" ] \
    && ok "outgoing server $OLD_SRV retains no loopback subsystem for the volume (F47 held)" \
    || note "F47 RESIDUE on $OLD_SRV: $(echo "$OLD_SHELLS" | tr '\n' ' ')"

  # (g) the volume is actually usable again, end to end.
  T_WITNESS=$(wait_witness_fresh 420)
  [ "$T_WITNESS" -ge 0 ] && ok "witness fresh at ${T_WITNESS}s" || note "witness NOT fresh in 420s"
  wait_acks_fresh 300 && T_RESUME=$(( $(epoch) - T0 )) || T_RESUME=-1
  witness_verdict "$T0"

  # (h) F52: the relocation must be INVISIBLE to the client. Pre-fix, the
  # new server's cold dcache resolved every replayed filehandle to "/"
  # (disconnected dentry), answering WRITE with EISDIR-as-IO — postgres
  # PANICs on the fdatasync EIO — and GETATTR with the container root's
  # attributes (client-side ESTALE, 1/s, server-silent). One Stale line
  # IS the regression (docs/f52-estale-on-rwx-server-relocation.md).
  # Both container instances: an eviction mid-drill erased the PANIC on
  # runai 3.6e and minted a hollow db=PASS.
  PGL_BOTH=$(printf '%s\n%s' \
    "$(kubectl logs -n "$NS" $PG -c postgres --since-time="$(rfc3339 "$T0")" 2>/dev/null || true)" \
    "$(kubectl logs -n "$NS" $PG -c postgres --previous 2>/dev/null || true)")
  F52_ESTALE=$(printf '%s' "$PGL_BOTH" | grep -ci "stale file handle" || true)
  F52_PANIC=$(printf '%s' "$PGL_BOTH" | grep -c "PANIC" || true)
  [ "${F52_ESTALE:-0}" -eq 0 ] && [ "${F52_PANIC:-0}" -eq 0 ] \
    && ok "client saw ZERO ESTALE / ZERO PANIC across the relocation (F52 held)" \
    || note "F52 SIGNATURE: estale=$F52_ESTALE panic=$F52_PANIC in the client's postgres log"
  # grep -c, NOT grep -q: this script runs under `set -o pipefail` (line 47)
  # and a server log is unbounded, so -q exits on the match, kubectl takes
  # SIGPIPE, and the pipeline reports failure — a FALSE NEGATIVE. It fired
  # on the very first fixed run (runaj 3.6f: the marker was there,
  # "1433 entries in 17.163963ms", and the drill said it was missing).
  PREWARM=$(kubectl logs -n "$DRIVER_NS" "${NEWPOD:-x}" 2>/dev/null \
    | grep -ac "fh identity index prewarmed")
  [ "${PREWARM:-0}" -gt 0 ] \
    && ok "new server prewarmed its fh identity index before serving" \
    || note "no fh-identity prewarm marker in the new server's log (pre-F52-fix image?)"

  # (i) Attribution artifact for the OPEN 3.6f mystery (runaj: pg-0 killed
  # at T0+8s, FailedKillPod/DeadlineExceeded, nobody claimed the kill —
  # state doc "open investigations" #1): kubelet's own journal from every
  # node pg-0 touched since T0, captured UNCONDITIONALLY so a passing run
  # still preserves the evidence a failing rerun will need.
  PG_NODE_NOW=$(kubectl get pod -n "$NS" $PG -o jsonpath='{.spec.nodeName}' 2>/dev/null)
  for n in $(printf '%s\n%s\n' "$PRE_NODE" "${PG_NODE_NOW:-}" | sort -u); do
    [ -n "$n" ] || continue
    capture_kubelet_log "$n" "$T0" "$ARTIFACTS/36f-kubelet-${n}.log" || continue
    KILLSIG=$(grep -aEc "FailedKillPod|Killing container|killPodWithSyncResult" \
      "$ARTIFACTS/36f-kubelet-${n}.log" || true)
    note "kubelet kill-signature lines on $n since T0: ${KILLSIG:-0}"
  done

  EXPECT_RESCHEDULE=none READY_TIMEOUT=180 \
    NOTES="F49 server-onto-leg: ${OLD_SRV}->${NEWNODE:-?} served=${T_SERVED}s eperm=$EPERM_HITS raidfail=$RAIDFAIL squatter='${LOCAL_LEG_EXPORT:-none}' f47_residue='$(echo "${OLD_SHELLS:-none}" | tr '\n' ' ')' raid='${RAID_POST}' witness=${T_WITNESS}s io_resume=${T_RESUME}s f52_estale=${F52_ESTALE} f52_panic=${F52_PANIC}" verify

  # Verdicts. (a) is the outage; (c)+(f) are the fixes' own invariants.
  [ "$T_SERVED" -ge 0 ] \
    || fail "3.6f FAIL (F49): the nfs server never became Ready on the leg-hosting node — raid assembly is wedged (EPERM hits=$EPERM_HITS); the volume is UNMOUNTABLE"
  [ "$NEWNODE" = "$TARGET" ] \
    || fail "3.6f INVALID: the server landed on ${NEWNODE:-?}, not the leg node $TARGET — the vector was not exercised"
  [ "$EPERM_HITS" = "0" ] && [ "$RAIDFAIL" = "0" ] \
    || fail "3.6f FAIL (F49): assembly hit the EPERM loop ('Operation not permitted'=$EPERM_HITS, 'bdev_raid_create failed'=$RAIDFAIL) even though the pod came Ready — the squatter race is still live"
  [ -z "$LOCAL_LEG_EXPORT" ] \
    || fail "3.6f FAIL (F49 fix 1): a leg export survives on the consumer's own node ($LOCAL_LEG_EXPORT) — it will squat the next assembly"
  echo "$RAID_POST" | grep -q "base=2/2" \
    || fail "3.6f FAIL: raid did not assemble 2/2 on $NEWNODE (${RAID_POST:-none})"
  [ -z "$OLD_SHELLS" ] \
    || fail "3.6f FAIL (F47): the outgoing server kept loopback subsystem(s) for the volume: $(echo "$OLD_SHELLS" | tr '\n' ' ')"
  [ "${F52_ESTALE:-0}" -eq 0 ] && [ "${F52_PANIC:-0}" -eq 0 ] \
    || fail "3.6f FAIL (F52): the relocation was NOT invisible to the client (estale=$F52_ESTALE panic=$F52_PANIC) — cold-dcache filehandle resolution regressed"
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

3.10) # F37 — same-node recreate races NodeUnstage: force-delete the nfs
      # pod ×3. After each cycle exactly ONE ublk id may serve any bdev on
      # the server node (the stage-side reap owns the dup), acks must stay
      # fresh (no EIO from reaping a live device), witness clean.
  pre_rwx
  DUPS_MAX=0; REAPS_TOTAL=0
  for i in $(seq 1 3); do
    C0=$(epoch); CUR_NODE=$(nfs_node); CUR_UID=$(nfs_pod_uid)
    kubectl delete pod -n "$DRIVER_NS" "$(nfs_pod)" --grace-period=0 --force --wait=false 2>/dev/null
    note "cycle $i: nfs pod force-deleted (grace 0) on $CUR_NODE"
    # reconciler recreate (disk-follows-pod → same node = the F37 window)
    for j in $(seq 1 36); do
      U=$(nfs_pod_uid)
      [ -n "$U" ] && [ "$U" != "$CUR_UID" ] \
        && [ "$(kubectl get pod -n "$DRIVER_NS" "$(nfs_pod)" -o jsonpath='{.status.phase}' 2>/dev/null)" = "Running" ] \
        && break
      sleep 5
    done
    NEW_NODE=$(nfs_node)
    [ "$NEW_NODE" = "$CUR_NODE" ] || note "cycle $i: recreate landed CROSS-node ($NEW_NODE) — F37 window not exercised this cycle"
    # settle window: the stranger may take a tick to reap
    DUPS=-1
    for j in $(seq 1 12); do
      DUPS=$(flint_ublk_disks "$NEW_NODE" | awk -F'\t' '{print $2}' | sort | uniq -d | grep -c . || true)
      [ "${DUPS:-1}" -eq 0 ] && break
      sleep 5
    done
    [ "$DUPS" -eq 0 ] && ok "cycle $i: no duplicate ublk ids on $NEW_NODE ($(( $(epoch) - C0 ))s)" \
      || note "cycle $i: DUPLICATE ublk ids persist on $NEW_NODE: $(flint_ublk_disks "$NEW_NODE" | tr '\n' ' ')"
    [ "${DUPS:-0}" -gt "$DUPS_MAX" ] && DUPS_MAX=$DUPS
    wait_acks_fresh 180 || note "cycle $i: acks not fresh in 180s"
  done
  REAPS_TOTAL=$(driver_log_hits "$T0" "F37: reaping same-bdev stranger")
  REFUSALS=$(driver_log_hits "$T0" "F37: ublk id .* still holds")
  note "F37 reap lines since T0: $REAPS_TOTAL; busy-refusals: $REFUSALS"
  witness_verdict "$T0"
  NOTES="F37 force-delete x3: dups_max=$DUPS_MAX reaps=$REAPS_TOTAL refusals=$REFUSALS" verify
  [ "$DUPS_MAX" -eq 0 ] || fail "3.10 FAIL: duplicate ublk id persisted past the settle window"
  ;;

3.11) # v1.21.0 RWX online expansion under writes.
  # ACCEPTANCE for the RWX orchestration (volume-expansion doc §3): patch
  # the USER RWX PVC +1Gi; the controller fans the backing-replica lvol
  # resize, patches the backing PV capacity, kubelet on the SERVER node
  # grows the fs (device-size guard first), and only then does the user PVC
  # complete. Clients need nothing — statfs comes from the server.
  # Hard gates: user PVC status, backing PVC status (kubelet stamps it only
  # AFTER the fs grew — it is the fs-growth proof), acks fresh, witness.
  pre_rwx
  PGPVC=$(kubectl get pod -n "$NS" $PG -o json | jq -r '.spec.volumes[] | select(.persistentVolumeClaim) | .persistentVolumeClaim.claimName' | head -1)
  [ -n "$PGPVC" ] || fail "no PVC behind $PG"
  BACKING_PVC="flint-nfs-pvc-$PV"
  SC=$(kubectl get pvc -n "$NS" "$PGPVC" -o jsonpath='{.spec.storageClassName}')
  [ "$(kubectl get sc "$SC" -o jsonpath='{.allowVolumeExpansion}' 2>/dev/null)" = "true" ] \
    || fail "SC $SC lacks allowVolumeExpansion:true — patch the SC first (the API refuses the PVC edit otherwise)"
  CUR=$(kubectl get pvc -n "$NS" "$PGPVC" -o jsonpath='{.status.capacity.storage}')
  case "$CUR" in *Gi) ;; *) fail "PVC capacity '$CUR' not in Gi — adjust the drill" ;; esac
  NEW="$(( ${CUR%Gi} + 1 ))Gi"
  NEW_BYTES=$(( (${CUR%Gi} + 1) * 1024 * 1024 * 1024 ))
  step "expanding RWX $PGPVC $CUR → $NEW under live writes (server $NFS_NODE, backing pvc $BACKING_PVC)"
  kubectl patch pvc -n "$NS" "$PGPVC" --type merge -p "{\"spec\":{\"resources\":{\"requests\":{\"storage\":\"$NEW\"}}}}" >/dev/null || fail "PVC patch refused"
  T_DONE=-1
  for i in $(seq 1 72); do
    ST=$(kubectl get pvc -n "$NS" "$PGPVC" -o jsonpath='{.status.capacity.storage}')
    [ "$ST" = "$NEW" ] && { T_DONE=$(( $(epoch) - T0 )); break; }
    sleep 5
  done
  if [ "$T_DONE" -lt 0 ]; then
    kubectl get events -n "$NS" --field-selector "involvedObject.name=$PGPVC" --no-headers | tail -5
    kubectl get pvc -n "$DRIVER_NS" "$BACKING_PVC" -o jsonpath='{.status.capacity.storage}{"\n"}' 2>/dev/null
    fail "RWX expansion did not complete in 360s (user pvc status=$ST)"
  fi
  ok "user PVC reports $NEW after ${T_DONE}s"
  BST_RAW=$(kubectl get pvc -n "$DRIVER_NS" "$BACKING_PVC" -o jsonpath='{.status.capacity.storage}' 2>/dev/null)
  case "$BST_RAW" in
    "$NEW"|"$NEW_BYTES") ok "backing PVC status grew to $BST_RAW (server fs verified grown by kubelet)" ;;
    *) fail "backing PVC status '$BST_RAW' != $NEW/$NEW_BYTES — user PVC completed WITHOUT the backing chain?!" ;;
  esac
  note "server-side df: $(kubectl exec -n "$DRIVER_NS" "$(nfs_pod)" -- sh -c 'df -m 2>/dev/null' | grep -m1 -E '/mnt|/export|/data' || echo 'n/a')"
  # The client mount is NOT a growth signal: a bare `df` does not list it, and
  # statfs on the export root comes back all-zeros, so the client sees "0" both
  # before and after. That is why the backing PVC status above is the gate.
  note "client-side df (informational, zeros are expected): $(kubectl exec -n "$NS" $PG -c postgres -- df -m /var/lib/postgresql/data 2>/dev/null | awk 'NR==2{print $1" "$2"M"}' || echo 'n/a')"
  wait_acks_fresh 60 || fail "acks stalled after expansion"
  witness_verdict "$T0"
  EXPECT_RESCHEDULE=none READY_TIMEOUT=120 \
    NOTES="RWX online expand ${CUR}->${NEW} in ${T_DONE}s under writes; backing=$BST_RAW" verify
  ;;

*) fail "unknown drill '$DRILL'" ;;
esac
