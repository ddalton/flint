#!/usr/bin/env bash
# Phase 2 drills — RWO, numReplicas=2 (harness on SC=flint-r2). One drill per
# invocation, same conventions as phase1.sh:
#
#   AWS_PROFILE=rolesanywhere KUBECONFIG=... ./phase2.sh 2.1
#
# The Phase-2 regression subset (1.2/1.3/1.4/1.6/1.9/1.10) reuses phase1.sh:
#   PHASE_LABEL=2 ./drills/phase1.sh 1.4
#
# New drills here:
#   2.1  csi-node POD delete on the REMOTE-leg node → degraded serve,
#        I/O uninterrupted (leaves the volume degraded: orchestrators are
#        default-off, so 2.5/2.6 chain onto this degraded state)
#   2.2a spdk-tgt PROCESS kill on the RAID-host node → repair_data_path
#        in-place reassembly (the v1.15.0-validated vector); needs both legs
#   2.2b csi-node POD delete on the RAID-host node → expected F8
#        reproduction on r2 (records blast radius; recovery = consumer bounce)
#   2.3  remote-leg NODE kill (kubelet stop + oos taint sim) → pod untouched
#   2.4 ☠ REAL terminate of pg's node + taint → reschedule served from the
#        surviving replica, ZERO lost acked writes (run supervised)
#   2.5  cross-node migration while degraded (run after 2.1)
#   2.6  churn ×10 while degraded (run after 2.1/2.5)
#   2.7  r3: kill BOTH remote legs simultaneously (needs r3 harness)
#   2.8 ☠ U11: terminate + delete the REMOTE-leg node → live replica
#        re-placement + full rebuild to in_sync (consumes a node)
#   2.9  F11: destroy the remote leg's lvstore → detection (3×60s) +
#        in-place re-init + catch-up rebuild to in_sync
#   2.10 v1.21.0 online expansion under writes (multi-replica fan-out;
#        run again after 2.1 to prove the degraded-refusal belt)
#   2.11 ALL-AT-ONCE: kill EVERY replica node's csi-node pod INCLUDING the
#        raid host's, simultaneously — 2.7 without its raid-host exclusion.
#        The supported all-at-once upgrade recipe in drill form: all-at-once
#        should be the CORRECTNESS-SAFEST path (no leg goes Stale because
#        nobody takes writes another misses), so the oracles are that
#        degraded-direct and acked-tail-risk NEVER appear at ANY sample.
#        Crosses the F62 shape on purpose — the raid host's tgt death
#        destroys the composition with `staged` still set, so the periodic
#        strike repair is the only inverse and recovery is MINUTES.
set -uo pipefail
cd "$(dirname "$0")/.."
. ./lib.sh

DRILL=${1:?drill id, e.g. 2.1}
PHASE_LABEL=${PHASE_LABEL:-2}

# ---- r2 topology helpers -----------------------------------------------

spdk_rpc() { # <node> <rpc args...> — spdk RPC via the node's spdk-tgt container
  local pod; pod=$(csi_node_pod "$1"); shift
  [ -n "$pod" ] || return 1
  kubectl exec -n "$DRIVER_NS" "$pod" -c spdk-tgt -- sh -c \
    "rpc.py $* 2>/dev/null || /usr/local/scripts/rpc.py $* 2>/dev/null || python3 /usr/local/scripts/rpc.py $* 2>/dev/null"
}

replica_nodes() { # <pv> — nodes whose spdk-tgt holds an lvol bdev for this PV
  local n hits
  for n in $(worker_nodes); do
    # grep -c, NOT grep -q — the lib.sh single_orchestrator rule applies
    # verbatim here: -q exits on the first match, `kubectl exec` takes
    # SIGPIPE, and `set -o pipefail` (top of this file) then reports the
    # whole pipeline failed, so the leg is silently dropped. It is a RACE
    # against kubectl's write, so it under-reports intermittently — which
    # reads as "the volume lost a leg" and fired 2.11's >=2 guard on a
    # cluster whose raid was demonstrably online base=2/2. Count instead:
    # counting consumes the stream, so there is no early exit to race.
    hits=$(spdk_rpc "$n" bdev_get_bdevs 2>/dev/null | grep -c "$1")
    [ "${hits:-0}" -gt 0 ] && echo "$n"
  done
}

controller_log_since() { # <t0> — controller (driver) log lines since t0
  # The claim registry and catch-up orchestrator are CONTROLLER-side, so the
  # F48 signatures live here, not in the per-node csi-node logs.
  local t; t=$(rfc3339 "$1")
  kubectl logs -n "$DRIVER_NS" -l app=flint-csi-controller --all-containers \
    --since-time="$t" --tail=-1 2>/dev/null \
    || kubectl logs -n "$DRIVER_NS" \
         "$(kubectl get pod -n "$DRIVER_NS" -o name 2>/dev/null | grep -m1 controller)" \
         --all-containers --since-time="$t" --tail=-1 2>/dev/null
}

raid_summary() { # <node> — compact raid bdev state on the node
  spdk_rpc "$1" bdev_raid_get_bdevs all 2>/dev/null \
    | jq -r '.[] | .name + " state=" + .state + " base=" + ((.base_bdevs_list // []) | map(select(.is_configured)) | length | tostring) + "/" + (.num_base_bdevs | tostring)' 2>/dev/null
}

spdk_restarts() { # <node> — spdk-tgt is a native-sidecar INIT container
  kubectl get pod -n "$DRIVER_NS" "$(csi_node_pod "$1")" \
    -o jsonpath='{.status.initContainerStatuses[?(@.name=="spdk-tgt")].restartCount}' 2>/dev/null
}

wait_acks_fresh() { # [budget_s] — until the ledger acks something NEWER than T0
  local budget=${1:-180} last now i
  for i in $(seq 1 $(( budget / 5 ))); do
    last=$(timeout 15 kubectl exec -n "$NS" "$(load_pod)" -- sh -c 'tail -1 /acked/acked.log 2>/dev/null' | awk '{print $2}')
    now=$(epoch)
    [ -n "$last" ] && [ "$last" -gt "${T0:-0}" ] && [ $(( now - last )) -lt 5 ] && return 0
    sleep 5
  done
  return 1
}

pre_r2() {
  need_env
  harness_healthy
  PRE_NODE=$(pg_node); PRE_UID=$(pg_uid); PRE_RESTARTS=$(pg_restarts)
  PRE_ORPHANS=$(orphan_data_paths | grep -c . || true)
  PV=$(pg_pv)
  RAID_HOST=$PRE_NODE
  REMOTE=$(replica_nodes "$PV" | grep -v "^$RAID_HOST$" | head -1)
  export PRE_NODE PRE_UID PRE_RESTARTS PRE_ORPHANS PV RAID_HOST REMOTE
  T0=$(epoch)
  step "T0=$T0 pg-0 on $RAID_HOST (pv ${PV:0:24}…, remote leg: ${REMOTE:-NONE-FOUND})"
  step "raid state pre: $(raid_summary "$RAID_HOST" | head -2)"
}

verify() { ./verify-drill.sh "$PHASE_LABEL" "$DRILL" "$T0"; }

evict_load_from() { # <node...> — the ledger oracle must survive the drill.
  # 2u/2.3 blinded itself: pg-load sat on the remote-leg node, the node
  # kill took acked.log (the loss ground truth) with it, and with every
  # other node cordoned/anti-affine the oracle went Pending. Relocate it
  # off the target nodes BEFORE the drill.
  local lp ln n hit=""
  lp=$(load_pod); [ -n "$lp" ] || return 0
  ln=$(kubectl get pod -n "$NS" "$lp" -o jsonpath='{.spec.nodeName}' 2>/dev/null)
  for n in "$@"; do [ "$ln" = "$n" ] && hit=1; done
  [ -n "$hit" ] || return 0
  note "ledger oracle on drill-target $ln — relocating (acked.log must survive)"
  for n in "$@"; do kubectl cordon "$n" >/dev/null 2>&1; done
  kubectl delete pod -n "$NS" "$lp" --wait=false
  # The old pod stays phase=Running through graceful termination and
  # load_pod happily returns it — wait for it to be GONE first, or the
  # "relocation" reports the terminating pod on the doomed node.
  kubectl wait --for=delete pod -n "$NS" "$lp" --timeout=120s >/dev/null 2>&1
  local i
  for i in $(seq 1 24); do
    lp=$(load_pod); [ -n "$lp" ] && break; sleep 5
  done
  for n in "$@"; do kubectl uncordon "$n" >/dev/null 2>&1; done
  [ -n "$lp" ] || fail "load pod never came back after relocation"
  ok "oracle relocated to $(kubectl get pod -n "$NS" "$lp" -o jsonpath='{.spec.nodeName}')"
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

2.1) # remote-leg csi-node POD delete → degraded serve, I/O uninterrupted
  pre_r2
  [ -n "$REMOTE" ] || fail "no remote replica node found for $PV"
  CNP=$(csi_node_pod "$REMOTE")
  kubectl delete pod -n "$DRIVER_NS" "$CNP" --wait=false
  note "csi-node pod on remote leg $REMOTE deleted (raid host $RAID_HOST untouched)"
  # I/O must never stop: poll for 90s, assert continuous acking
  WORST=0
  for i in $(seq 1 18); do
    last=$(timeout 15 kubectl exec -n "$NS" "$(load_pod)" -- sh -c 'tail -1 /acked/acked.log 2>/dev/null' | awk '{print $2}')
    age=$(( $(epoch) - ${last:-0} ))
    [ "$age" -gt "$WORST" ] && WORST=$age
    sleep 5
  done
  [ "$WORST" -le 15 ] && ok "I/O uninterrupted through remote-leg loss (worst ack age ${WORST}s)" \
                      || note "I/O hiccup: worst ack age ${WORST}s"
  note "raid state post: $(raid_summary "$RAID_HOST" | head -2)"
  EXPECT_RESCHEDULE=none READY_TIMEOUT=60 \
    NOTES="remote-leg csi-node delete; worst_ack_age=${WORST}s; volume left DEGRADED" verify
  ;;

2.2a) # spdk-tgt PROCESS kill on RAID host → repair_data_path in-place
  pre_r2
  IID=$(instance_id_for_node "$RAID_HOST")
  [ -n "$IID" ] || fail "no instance id for $RAID_HOST"
  SPDK_PRE=$(spdk_restarts "$RAID_HOST")
  ssm_run "$IID" "pkill -9 -f /usr/local/bin/spdk_tgt" >/dev/null
  note "spdk-tgt SIGKILL on RAID host $RAID_HOST"
  for i in $(seq 1 24); do
    [ "$(spdk_restarts "$RAID_HOST")" != "$SPDK_PRE" ] && break; sleep 5
  done
  [ "$(spdk_restarts "$RAID_HOST")" != "$SPDK_PRE" ] || fail "spdk-tgt never restarted — kill failed?"
  ok "spdk-tgt container restarted"
  wait_acks_fresh 300 && T_RESUME=$(( $(epoch) - T0 )) || T_RESUME=-1
  [ "$T_RESUME" -ge 0 ] && ok "I/O resumed ${T_RESUME}s after kill (repair_data_path)" \
                        || note "I/O did NOT resume in 300s"
  note "raid state post: $(raid_summary "$RAID_HOST" | head -2)"
  EXPECT_RESCHEDULE=none READY_TIMEOUT=300 \
    NOTES="RAID-host spdk-tgt kill; io_resume=${T_RESUME}s; raid=$(raid_summary "$RAID_HOST" | head -1)" verify
  ;;

2.2b) # csi-node POD delete on RAID host → expected F8 on r2; recovery = bounce
  pre_r2
  CNP=$(csi_node_pod "$RAID_HOST")
  kubectl delete pod -n "$DRIVER_NS" "$CNP" --wait=false
  note "csi-node POD on RAID host $RAID_HOST deleted (F8 probe: expect no self-recovery)"
  # The old pod's tgt keeps serving through graceful termination (~30s) —
  # measuring acks from T0 races it (the first run recorded a bogus
  # "resumed 1s" while the kill hadn't landed). Outage starts when the
  # old pod is GONE; acks must be newer than that.
  kubectl wait --for=delete pod -n "$DRIVER_NS" "$CNP" --timeout=180s >/dev/null 2>&1
  T_GONE=$(epoch)
  note "old csi-node pod fully terminated at +$(( T_GONE - T0 ))s (outage starts)"
  kubectl wait --for=condition=Ready pod -l app=flint-csi-node -n "$DRIVER_NS" \
    --field-selector "spec.nodeName=$RAID_HOST" --timeout=180s >/dev/null 2>&1
  ORIG_T0=$T0; T0=$T_GONE
  RESUMED=0; wait_acks_fresh 300 && RESUMED=1
  T0=$ORIG_T0
  if [ "$RESUMED" = "1" ]; then
    T_RESUME=$(( $(epoch) - T0 ))
    ok "I/O resumed ${T_RESUME}s — BETTER than F8 predicts (r2 divergence, record it)"
    EXPECT_RESCHEDULE=none READY_TIMEOUT=120 NOTES="RAID-host csi-node POD delete: SELF-RECOVERED io_resume=${T_RESUME}s" verify
  else
    note "I/O dead at 300s — F8 reproduced on r2; recovering via consumer bounce"
    RUID=$(pg_uid)
    kubectl delete pod -n "$NS" $PG --grace-period=0 --force --wait=false
    wait_pod_replaced "$NS" $PG "$RUID" 600 || fail "bounce recovery failed"
    wait_acks_fresh 300 || fail "I/O never resumed after bounce"
    T_REC=$(( $(epoch) - T0 ))
    READY_TIMEOUT=120 NOTES="RAID-host csi-node POD delete: F8 reproduced on r2; recovery=consumer bounce, total ${T_REC}s" verify
  fi
  ;;

2.3) # remote-leg NODE kill (kubelet stop + oos taint) → pod untouched
  pre_r2
  [ -n "$REMOTE" ] || fail "no remote replica node found for $PV"
  evict_load_from "$REMOTE"
  IID=$(instance_id_for_node "$REMOTE")
  [ -n "$IID" ] || fail "no instance id for $REMOTE"
  kubelet_stop "$REMOTE"; DEAD_IID="$IID"
  wait_node_notready "$REMOTE" 180 || fail "remote node never NotReady"
  taint_oos "$REMOTE"; TAINTED="$REMOTE"
  note "remote node $REMOTE dead+tainted; watching I/O for 120s"
  WORST=0
  for i in $(seq 1 24); do
    last=$(timeout 15 kubectl exec -n "$NS" "$(load_pod)" -- sh -c 'tail -1 /acked/acked.log 2>/dev/null' | awk '{print $2}')
    age=$(( $(epoch) - ${last:-0} ))
    [ "$age" -gt "$WORST" ] && WORST=$age
    sleep 5
  done
  [ "$WORST" -le 30 ] && ok "I/O rode through remote node death (worst ack age ${WORST}s)" \
                      || note "I/O impact: worst ack age ${WORST}s"
  EXPECT_RESCHEDULE=none READY_TIMEOUT=60 \
    NOTES="remote-leg node kill (sim): worst_ack_age=${WORST}s" verify
  untaint_oos "$REMOTE"; TAINTED=""
  kubelet_start_ssm "$IID"; DEAD_IID=""
  wait_node_ready "$REMOTE" 300 && ok "remote node restored" || note "remote node not Ready — check kubelet"
  ;;

2.4) # ☠ REAL terminate of pg's node — the headline r2 drill (run supervised)
  pre_r2
  IID=$(instance_id_for_node "$RAID_HOST")
  [ -n "$IID" ] || fail "no instance id for $RAID_HOST"
  aws ec2 terminate-instances --region "$AWS_REGION" --instance-ids "$IID" >/dev/null
  note "TERMINATED $IID ($RAID_HOST) — data must survive on $REMOTE"
  wait_node_notready "$RAID_HOST" 300
  T_NOTREADY=$(( $(epoch) - T0 ))
  taint_oos "$RAID_HOST"; TAINTED="$RAID_HOST"
  wait_pod_replaced "$NS" $PG "$PRE_UID" 600 || fail "replacement never Ready after taint"
  wait_acks_fresh 300 && T_RESUME=$(( $(epoch) - T0 )) || T_RESUME=-1
  EXPECT_RESCHEDULE=cross READY_TIMEOUT=60 \
    NOTES="r2 NODE TERMINATE: notready=${T_NOTREADY}s io_resume=${T_RESUME}s served from surviving replica" verify
  untaint_oos "$RAID_HOST"; TAINTED=""
  kubectl delete node "$RAID_HOST" >/dev/null 2>&1
  note "NEXT: scale a replacement worker via trove; volume now single-replica"
  ;;

2.5) # cross-node migration while degraded (run after 2.1)
  pre_r2
  kubectl cordon "$PRE_NODE" >/dev/null; CORDONED="$PRE_NODE"
  kubectl delete pod -n "$NS" $PG --wait=false
  wait_pod_replaced "$NS" $PG "$PRE_UID" 400 || fail "replacement never Ready"
  kubectl uncordon "$PRE_NODE" >/dev/null; CORDONED=""
  NEW_HOST=$(pg_node)
  note "raid state on new host $NEW_HOST: $(raid_summary "$NEW_HOST" | head -2)"
  EXPECT_RESCHEDULE=cross NOTES="cross-node migration while DEGRADED" verify
  ;;

2.6) # churn ×10 while degraded
  pre_r2
  for i in $(seq 1 10); do
    CUR_NODE=$(pg_node); CUR_UID=$(pg_uid); C0=$(epoch)
    if [ $(( i % 2 )) -eq 0 ]; then
      kubectl cordon "$CUR_NODE" >/dev/null; CORDONED="$CUR_NODE"
    fi
    kubectl delete pod -n "$NS" $PG --wait=false
    wait_pod_replaced "$NS" $PG "$CUR_UID" 300 || fail "cycle $i: replacement never Ready"
    [ -n "$CORDONED" ] && { kubectl uncordon "$CORDONED" >/dev/null; CORDONED=""; }
    note "cycle=$i mode=$([ $(( i % 2 )) -eq 0 ] && echo cross || echo same) secs=$(( $(epoch) - C0 )) node=$(pg_node)"
  done
  TOT_GM=0
  for n in $(worker_nodes); do TOT_GM=$(( TOT_GM + $(globalmounts "$n") )); done
  N_VA=$(kubectl get volumeattachments --no-headers 2>/dev/null | wc -l | tr -d ' ')
  [ "$TOT_GM" -eq 1 ] || note "LEAK? total globalmounts=$TOT_GM (want 1)"
  [ "$N_VA" -eq 1 ] || note "LEAK? volumeattachments=$N_VA (want 1)"
  NOTES="churn x10 degraded; tot_gm=$TOT_GM vas=$N_VA" verify
  ;;

2.7) # r3: kill BOTH remote legs simultaneously → serve continues on the
     # single local leg; both legs must re-join (survivable reconnect)
  pre_r2
  REMOTES=$(replica_nodes "$PV" | grep -v "^$RAID_HOST$")
  N_REMOTES=$(echo "$REMOTES" | grep -c .)
  [ "$N_REMOTES" -ge 2 ] || fail "need >=2 remote legs (r3 harness) — found $N_REMOTES"
  # No oracle relocation: 2.7 deletes csi-node PODS only — pg-load is not
  # a storage consumer and survives; cordoning every remote on a tight
  # fleet strands the oracle instead (nvmeof r3 run).
  for r in $REMOTES; do
    CNP=$(csi_node_pod "$r")
    kubectl delete pod -n "$DRIVER_NS" "$CNP" --wait=false
    note "csi-node pod on remote leg $r deleted"
  done
  WORST=0
  for i in $(seq 1 24); do
    last=$(timeout 15 kubectl exec -n "$NS" "$(load_pod)" -- sh -c 'tail -1 /acked/acked.log 2>/dev/null' | awk '{print $2}')
    age=$(( $(epoch) - ${last:-0} ))
    [ "$age" -gt "$WORST" ] && WORST=$age
    sleep 5
  done
  [ "$WORST" -le 30 ] && ok "I/O rode through DOUBLE remote-leg loss (worst ack age ${WORST}s)" \
                      || note "I/O impact: worst ack age ${WORST}s"
  note "raid state post: $(raid_summary "$RAID_HOST" | head -2)"
  EXPECT_RESCHEDULE=none READY_TIMEOUT=60 \
    NOTES="r3 DOUBLE remote-leg kill; worst_ack_age=${WORST}s; raid=$(raid_summary "$RAID_HOST" | head -1)" verify
  ;;

2.8) # ☠ U11: REAL terminate of the REMOTE-leg node + Node delete → the
     # controller re-places the leg on a healthy node and rebuilds
     # redundancy LIVE (raid keeps serving degraded; pg never restarts).
     # Consumes a node — run on a fleet with a spare storage node.
  pre_r2
  [ -n "$REMOTE" ] || fail "no remote leg found"
  evict_load_from "$REMOTE"
  IID=$(instance_id_for_node "$REMOTE")
  [ -n "$IID" ] || fail "no instance id for $REMOTE"
  OVR_PRE=$(kubectl get pv "$PV" -o jsonpath='{.metadata.annotations.disk\.chert\.us/replicas-override}' 2>/dev/null)
  [ -z "$OVR_PRE" ] || note "override already present pre-drill (prior replacement)"
  aws ec2 terminate-instances --region "$AWS_REGION" --instance-ids "$IID" >/dev/null
  note "TERMINATED $IID ($REMOTE) — remote leg permanently lost"
  wait_node_notready "$REMOTE" 300
  kubectl delete node "$REMOTE" >/dev/null 2>&1
  T_NODEGONE=$(( $(epoch) - T0 ))
  note "Node object deleted at ${T_NODEGONE}s — U11 trigger armed (catch-up tick 60s)"
  T_SWAP=-1; NEW_NODE=""
  for i in $(seq 1 60); do
    OVR=$(kubectl get pv "$PV" -o jsonpath='{.metadata.annotations.disk\.chert\.us/replicas-override}' 2>/dev/null)
    if [ -n "$OVR" ] && [ "$OVR" != "$OVR_PRE" ]; then
      T_SWAP=$(( $(epoch) - T0 ))
      NEW_NODE=$(echo "$OVR" | jq -r '.[].node_name' 2>/dev/null | grep -v "^$RAID_HOST$" | head -1)
      break
    fi
    sleep 10
  done
  [ "$T_SWAP" -ge 0 ] || fail "replicas-override never appeared — U11 did not fire"
  ok "identity swapped to ${NEW_NODE:-?} at ${T_SWAP}s"
  T_SYNC=-1; ST=""
  for i in $(seq 1 120); do
    REC=$(kubectl get pv "$PV" -o jsonpath='{.metadata.annotations.disk\.chert\.us/replica-sync-state}' 2>/dev/null)
    ST=$(echo "$REC" | jq -r --arg n "$NEW_NODE" '.replicas[] | select(.node_name==$n) | .sync_state' 2>/dev/null | head -1)
    [ "$ST" = "in_sync" ] && { T_SYNC=$(( $(epoch) - T0 )); break; }
    sleep 10
  done
  if [ "$T_SYNC" -ge 0 ]; then
    ok "replacement leg in_sync at ${T_SYNC}s — redundancy restored LIVE (no restage)"
  else
    note "replacement leg NOT in_sync after 20min (state: ${ST:-unknown}) — record + investigate"
  fi
  wait_acks_fresh 60 || note "acks not fresh at drill end"
  note "raid state post: $(raid_summary "$RAID_HOST" | head -2)"
  EXPECT_RESCHEDULE=none READY_TIMEOUT=60 \
    NOTES="U11 re-placement: node_gone=${T_NODEGONE}s swap=${T_SWAP}s in_sync=${T_SYNC}s new_node=${NEW_NODE:-?}" verify
  note "fleet is one storage node down — replace via trove before node-consuming drills"
  ;;

2.9) # F11: destroy the REMOTE leg's lvstore in place → the node agent
     # detects the lost store (3×60s strikes vs PV ground truth),
     # re-inits it (FLINT_STORE_REINIT default-on; all expecting volumes
     # multi-replica), and catch-up full-builds the leg back to in_sync.
     # Node survives; no operator action; oracle rides through degraded.
  pre_r2
  [ -n "$REMOTE" ] || fail "no remote leg found"
  evict_load_from "$REMOTE"
  LVS=$(kubectl get pv "$PV" -o json 2>/dev/null \
    | jq -r --arg n "$REMOTE" '(.metadata.annotations["disk.chert.us/replicas-override"] // .spec.csi.volumeAttributes["disk.chert.us/replicas"]) | fromjson | .[] | select(.node_name==$n) | .lvs_name' | head -1)
  [ -n "$LVS" ] || fail "no lvs_name for remote leg on $REMOTE"
  UUID_PRE=$(kubectl get pv "$PV" -o json 2>/dev/null \
    | jq -r --arg n "$REMOTE" '(.metadata.annotations["disk.chert.us/replicas-override"] // .spec.csi.volumeAttributes["disk.chert.us/replicas"]) | fromjson | .[] | select(.node_name==$n) | .lvol_uuid' | head -1)
  spdk_rpc "$REMOTE" bdev_lvol_delete_lvstore -l "$LVS" \
    || fail "could not delete lvstore $LVS on $REMOTE (drill needs direct RPC access)"
  note "lvstore $LVS on $REMOTE DESTROYED (F11 simulation: store gone, node alive)"
  T_REINIT=-1
  for i in $(seq 1 60); do  # detection needs 3 monitor ticks (~3-4min)
    if spdk_rpc "$REMOTE" bdev_lvol_get_lvstores 2>/dev/null | grep -q "\"$LVS\""; then
      T_REINIT=$(( $(epoch) - T0 )); break
    fi
    sleep 10
  done
  [ "$T_REINIT" -ge 0 ] || fail "store never re-initialized — F11 self-heal did not fire"
  ok "store re-initialized in place at ${T_REINIT}s"
  # F50 gate: a parked standby that flips to stale means a hot-rejoin
  # window opened on it (the intent write demotes standby BY DESIGN); the
  # leg bouncing standby→stale→standby repeatedly without ever reaching
  # in_sync is the F50 livelock (windows opened by one controller process
  # scrubbed mid-flight by another — the vestigial operator pod on runai).
  # Fail it BY NAME instead of timing out generically — a silent 15-min
  # timeout is what let F50 hide behind F48.
  T_SYNC=-1; ST=""; PREV_ST=""; F50_CYCLES=0
  for i in $(seq 1 90); do
    REC=$(kubectl get pv "$PV" -o jsonpath='{.metadata.annotations.disk\.chert\.us/replica-sync-state}' 2>/dev/null)
    ST=$(echo "$REC" | jq -r --arg n "$REMOTE" '.replicas[] | select(.node_name==$n) | .sync_state' 2>/dev/null | head -1)
    [ "$ST" = "in_sync" ] && { T_SYNC=$(( $(epoch) - T0 )); break; }
    if [ "$PREV_ST" = "standby" ] && [ "$ST" = "stale" ]; then
      F50_CYCLES=$(( F50_CYCLES + 1 ))
      note "standby→stale flip #$F50_CYCLES (a hot-rejoin window opened on the parked leg)"
      [ "$F50_CYCLES" -gt 2 ] && fail "F50: the leg is cycling standby→stale→standby ($F50_CYCLES windows opened, none landed) — admission windows are being destroyed mid-flight (docs/f50-hotrejoin-window-concurrency.md)"
    fi
    PREV_ST="$ST"
    sleep 10
  done
  if [ "$T_SYNC" -ge 0 ]; then
    ok "rebuilt leg in_sync at ${T_SYNC}s — store loss fully self-healed (window flips: $F50_CYCLES)"
  else
    note "rebuilt leg NOT in_sync after 15min (state: ${ST:-unknown}, window flips: $F50_CYCLES) — record + investigate"
  fi
  EV=$(kubectl get events -n default --field-selector reason=ReplicaStoreReinitialized --no-headers 2>/dev/null | grep -c . || true)
  [ "${EV:-0}" -ge 1 ] && ok "ReplicaStoreReinitialized event emitted" || note "no ReplicaStoreReinitialized event found"
  wait_acks_fresh 60 || note "acks not fresh at drill end"
  note "raid state post: $(raid_summary "$RAID_HOST" | head -2)"

  # ---- F48 gate (runah 2.9 run A) ---------------------------------------
  # Run A wedged this exact drill at 1/2 forever. Three signatures, each
  # tied to one fix in docs/f48-standby-rejoin-epoch-race.md:
  CLOG=$(controller_log_since "$T0")
  HEADINUSE=$(echo "$CLOG" | grep -c "ReplicaHeadInUse\|stale head is LIVE-CONSUMED" || true)
  ZOMBIE=$(echo "$CLOG" | grep -c "severing zombie consumer" || true)
  # fix 2: a resolver that finds no work must hand the reservation back, so
  # no maintainer should ever yield for more than about one 60s tick.
  MAXRES=$(echo "$CLOG" | grep -o "reserved_secs=[0-9]*" | cut -d= -f2 | sort -n | tail -1)
  MAXRES=${MAXRES:-0}
  RELEASED=$(echo "$CLOG" | grep -c "released its reservation" || true)
  # fix 3: the -32603 duplicate shape must be adopted, not fatal.
  EFDUP=$(echo "$CLOG" | grep -c "Unable to create subsystem" || true)

  [ "$ZOMBIE" = "0" ] \
    && note "no zombie consumers needed severing this run" \
    || ok "F48 fix 1 FIRED: severed $ZOMBIE zombie consumer(s) — pre-fix this run would have wedged at 1/2"
  [ "$MAXRES" -le 65 ] \
    && ok "no reservation starvation (max reserved_secs=${MAXRES}s, releases=$RELEASED)" \
    || note "RESERVATION STARVATION: a maintainer yielded up to ${MAXRES}s (releases=$RELEASED) — F48 fix 2 shape"
  [ "$EFDUP" = "0" ] \
    && ok "no E_f duplicate-create errors" \
    || note "E_f duplicate shape seen ${EFDUP}x — adopted if the rebuild still converged (F48 fix 3)"

  EXPECT_RESCHEDULE=none READY_TIMEOUT=60 \
    NOTES="F11 self-heal: reinit=${T_REINIT}s in_sync=${T_SYNC}s lvs=$LVS old_uuid=${UUID_PRE:0:8}; F48: head_in_use=$HEADINUSE zombies_severed=$ZOMBIE max_reserved=${MAXRES}s releases=$RELEASED efdup=$EFDUP" verify

  # THE F48 VERDICT: run A ended here with in_sync never reached, the raid
  # slot unconfigured, and the F36 defer looping on a zombie controller.
  [ "$T_SYNC" -ge 0 ] \
    || fail "2.9 FAIL (F48): the rebuilt leg never reached in_sync (state='${ST:-unknown}') — head_in_use=$HEADINUSE zombies_severed=$ZOMBIE max_reserved=${MAXRES}s; the volume is stuck degraded 1/2 with no self-heal"
  # NOT a hard gate: a high reserved_secs is only starvation if it BLOCKED the
  # rebuild, and the in_sync assertion above already proves it did not. A
  # resolver legitimately holding its reservation across a long catch-up shows
  # the same number, so failing on it alone red-flags healthy runs.
  [ "$MAXRES" -le 65 ] \
    || note "max reserved_secs=${MAXRES}s exceeded one tick (releases=$RELEASED) — benign here since the leg still reached in_sync, but worth a look if it grows"
  ;;

2.10) # v1.21.0 online expansion under writes (multi-replica RWO).
  # ACCEPTANCE for the expand fan-out (volume-expansion doc §2): patch the
  # PVC +1Gi while pg writes; the resizer→controller fan-out grows every
  # leg, SPDK propagates lvol→nvmf→bdev_nvme→raid automatically, kubelet's
  # NodeExpand passes the device-size guard and grows the fs. Assert the
  # END-TO-END truth (PVC status + df inside the consumer), not the plumbing.
  # Variant: run after 2.1 (degraded r2) — the expand must REFUSE (event
  # ExpandRefusedReplicasNotInSync, PVC stays pending, I/O unaffected) and
  # then complete on its own after the leg rejoins.
  pre_r2
  PGPVC=$(kubectl get pod -n "$NS" $PG -o json | jq -r '.spec.volumes[] | select(.persistentVolumeClaim) | .persistentVolumeClaim.claimName' | head -1)
  [ -n "$PGPVC" ] || fail "no PVC behind $PG"
  VOLNAME=$(kubectl get pod -n "$NS" $PG -o json | jq -r --arg c "$PGPVC" '.spec.volumes[] | select(.persistentVolumeClaim.claimName==$c) | .name')
  MP=$(kubectl get pod -n "$NS" $PG -o json | jq -r --arg v "$VOLNAME" '.spec.containers[0].volumeMounts[] | select(.name==$v) | .mountPath')
  DF_PRE=$(kubectl exec -n "$NS" $PG -- df -m "$MP" 2>/dev/null | awk 'NR==2{print $2}')
  SC=$(kubectl get pvc -n "$NS" "$PGPVC" -o jsonpath='{.spec.storageClassName}')
  [ "$(kubectl get sc "$SC" -o jsonpath='{.allowVolumeExpansion}' 2>/dev/null)" = "true" ] \
    || fail "SC $SC lacks allowVolumeExpansion:true — patch the SC first (the API refuses the PVC edit otherwise)"
  CUR=$(kubectl get pvc -n "$NS" "$PGPVC" -o jsonpath='{.status.capacity.storage}')
  case "$CUR" in *Gi) ;; *) fail "PVC capacity '$CUR' not in Gi — adjust the drill" ;; esac
  NEW="$(( ${CUR%Gi} + 1 ))Gi"
  step "expanding $PGPVC $CUR → $NEW under live writes (mount $MP, df_pre=${DF_PRE}M)"
  kubectl patch pvc -n "$NS" "$PGPVC" --type merge -p "{\"spec\":{\"resources\":{\"requests\":{\"storage\":\"$NEW\"}}}}" >/dev/null || fail "PVC patch refused"
  T_DONE=-1
  for i in $(seq 1 60); do
    ST=$(kubectl get pvc -n "$NS" "$PGPVC" -o jsonpath='{.status.capacity.storage}')
    [ "$ST" = "$NEW" ] && { T_DONE=$(( $(epoch) - T0 )); break; }
    sleep 5
  done
  if [ "$T_DONE" -lt 0 ]; then
    kubectl get events -n "$NS" --field-selector "involvedObject.name=$PGPVC" --no-headers | tail -5
    kubectl get events -n default --field-selector reason=ExpandRefusedReplicasNotInSync --no-headers 2>/dev/null | tail -3
    fail "expansion did not complete in 300s (status=$ST) — if the volume is degraded this is the DESIGNED refusal; heal the leg and re-check"
  fi
  ok "PVC reports $NEW after ${T_DONE}s"
  DF_POST=$(kubectl exec -n "$NS" $PG -- df -m "$MP" 2>/dev/null | awk 'NR==2{print $2}')
  [ "${DF_POST:-0}" -gt "${DF_PRE:-0}" ] && ok "consumer filesystem grew ${DF_PRE}M → ${DF_POST}M" \
    || fail "filesystem did NOT grow (${DF_PRE}M → ${DF_POST:-?}M) — device-size guard should have refused NodeExpand; investigate"
  for n in $RAID_HOST $REMOTE; do
    # On a leg host the lvol is ALIASED "<lvs>/<pv>"; on the raid host the raid
    # and its nvme legs carry the pv in the bdev NAME and are aliased by uuid —
    # match either or the raid host silently reports "n/a".
    [ -n "$n" ] && note "leg bytes on $n: $(spdk_rpc "$n" bdev_get_bdevs 2>/dev/null | jq -r --arg pv "$PV" '[.[] | select((.name | contains($pv)) or ((.aliases // [])[]? | contains($pv))) | .block_size * .num_blocks] | max // "n/a"')"
  done
  note "raid state post: $(raid_summary "$RAID_HOST" | head -2)"
  wait_acks_fresh 60 || fail "acks stalled after expansion"
  EXPECT_RESCHEDULE=none READY_TIMEOUT=60 \
    NOTES="online expand ${CUR}->${NEW} in ${T_DONE}s under writes; df ${DF_PRE}M->${DF_POST}M; raid=$(raid_summary "$RAID_HOST" | head -1)" verify
  ;;

2.11) # ALL-AT-ONCE: kill EVERY replica node's csi-node pod INCLUDING the raid
  # host's, simultaneously.  2.7 does this for the REMOTE legs only (it greps
  # the raid host out); dropping that exclusion is the entire difference, and
  # it changes the drill from "can the raid ride out losing its legs" to "can
  # the VOLUME come back when every tgt under it dies at once".
  #
  # WHY THIS SHAPE IS WORTH TESTING: it is the supported all-at-once upgrade
  # recipe (pre-pull -> patch maxUnavailable="100%" -> helm upgrade -> revert),
  # and on paper all-at-once is the CORRECTNESS-SAFEST path — simultaneous tgt
  # death means nobody takes writes another misses, so no leg goes Stale and
  # full membership is available on recovery.  A rolling upgrade, by contrast,
  # always makes the rolled leg stale.  This drill is what turns that argument
  # into a supported procedure, or refutes it.
  #
  # It also crosses the F62 shape deliberately: killing the RAID HOST's tgt
  # destroys the composition while `staged` stays set, so NodeStage is never
  # called again and the shipped periodic strike repair (repair_data_path, 2
  # strikes) is the only inverse.  Recovery is therefore expected in MINUTES,
  # not seconds.
  #
  # NEVER --force: a forced delete converts the safe UBLK_F_USER_RECOVERY
  # quiesce into the DEAD case and costs the mount (runy2/runz).
  pre_r2
  ALL=$(replica_nodes "$PV")
  N_ALL=$(echo "$ALL" | grep -c .)
  [ "$N_ALL" -ge 2 ] || fail "need >=2 replica legs — found $N_ALL"
  echo "$ALL" | grep -q "^$RAID_HOST$" \
    || fail "2.11 requires the raid host among the replica nodes (got: $(echo $ALL)) — without it this is just 2.7"
  EXPECT_BASE="$N_ALL"
  note "killing csi-node on ALL $N_ALL replica nodes INCLUDING raid host $RAID_HOST"

  dd_ann()   { kubectl get pv "$PV" -o jsonpath='{.metadata.annotations.chert\.us/degraded-direct}'   2>/dev/null; }
  risk_ann() { kubectl get pv "$PV" -o jsonpath='{.metadata.annotations.chert\.us/acked-tail-risk}'  2>/dev/null; }

  for n in $ALL; do note "spdk-tgt restarts pre on $n: $(spdk_restarts "$n")"; done
  for n in $ALL; do
    kubectl delete pod -n "$DRIVER_NS" "$(csi_node_pod "$n")" --wait=false
    note "csi-node pod on $n deleted"
  done

  # SAMPLE THROUGHOUT — never at the end.  chert.us/degraded-direct is CLEARED
  # by the next >=2-leg assembly (driver.rs, the annotation-homing comment), so
  # a single post-hoc check would find it absent precisely BECAUSE the volume
  # recovered through a degraded serve.  That is the checkpoint-phase luck that
  # hid F55 in every earlier 3.6e run.  Same for the raid base count.
  DD_SEEN=""; RISK_SEEN=""; WORST=0; BACK=""; TORN=""
  for i in $(seq 1 60); do   # 60 x 5s = 5 min, sized for the strike ladder
    D=$(dd_ann); [ -n "$D" ] && [ -z "$DD_SEEN" ] && { DD_SEEN="$D"; note "degraded-direct OBSERVED at t+$((i*5))s: $D"; }
    R=$(risk_ann); [ -n "$R" ] && [ -z "$RISK_SEEN" ] && { RISK_SEEN="$R"; note "acked-tail-risk OBSERVED at t+$((i*5))s: $R"; }
    last=$(timeout 15 kubectl exec -n "$NS" "$(load_pod)" -- sh -c 'tail -1 /acked/acked.log 2>/dev/null' | awk '{print $2}')
    age=$(( $(epoch) - ${last:-0} )); [ "$age" -gt "$WORST" ] && WORST=$age
    B=$(raid_summary "$RAID_HOST" 2>/dev/null | head -1)
    # TWO-PHASE LATCH — the composition must be observed GONE before any
    # $EXPECT_BASE/$EXPECT_BASE reading may be counted as RECOVERY.
    #
    # The delete is graceful by design (never --force), so for the first tens
    # of seconds the OLD spdk-tgt is still up and still answering this very
    # RPC with the PRE-KILL state.  A one-phase latch therefore records
    # "recovered" from a reading taken before anything died: runaq's first run
    # reported base_back=5s when the replacement tgt did not start until t+35s
    # and the in-place repair did not report success until t+170s.  It cannot
    # be back 30s before the process that rebuilds it exists.
    #
    # That is the SAME checkpoint-phase luck this drill's header calls out for
    # degraded-direct, reappearing in the recovery oracle: a pass earned by
    # sampling the wrong side of the event.  Requiring TORN first makes the
    # number mean "time to rebuild" instead of "time to notice nothing yet".
    case "$B" in
      *"base=$EXPECT_BASE/$EXPECT_BASE"*)
        [ -n "$TORN" ] && [ -z "$BACK" ] \
          && { BACK=$((i*5)); note "base back to $EXPECT_BASE/$EXPECT_BASE at t+${BACK}s (torn at t+${TORN}s)"; } ;;
      *)
        [ -z "$TORN" ] && { TORN=$((i*5)); note "composition observed GONE at t+${TORN}s (raid: ${B:-unreachable})"; } ;;
    esac
    sleep 5
  done

  # ORACLES.  The first two are the point of the drill: all-at-once is only a
  # supported recipe if the volume never degrades to a single-survivor DIRECT
  # serve and never has to surface an acked tail.
  [ -z "$DD_SEEN" ]   && ok "no degraded-direct at any sample (never fell to single-survivor direct serve)" \
                      || fail "2.11 FAIL: degraded-direct raised — all-at-once DID lose redundancy: $DD_SEEN"
  [ -z "$RISK_SEEN" ] && ok "no acked-tail-risk at any sample" \
                      || fail "2.11 FAIL: acked-tail-risk raised — acked writes were at risk: $RISK_SEEN"
  # Three outcomes, not two.  A run that never saw the composition go down has
  # not shown recovery is fast — it has shown the sampler missed the event, and
  # that must not be reported as a pass (it is how base_back=5s got into
  # results.csv as if it were a measurement).
  if [ -z "$TORN" ]; then
    fail "2.11 INCONCLUSIVE: composition never observed torn down — every sample read $EXPECT_BASE/$EXPECT_BASE, so recovery was never witnessed (stale RPC from the terminating tgt, or the kill did not land). Worst ack age ${WORST}s"
  elif [ -n "$BACK" ]; then
    ok "composition back to $EXPECT_BASE/$EXPECT_BASE in ${BACK}s (torn at t+${TORN}s, rebuild took $((BACK - TORN))s)"
  else
    fail "2.11 FAIL: torn at t+${TORN}s but base never returned to $EXPECT_BASE/$EXPECT_BASE within 300s (last: $(raid_summary "$RAID_HOST" | head -1))"
  fi
  note "worst ack age ${WORST}s (ublk holds I/O rather than erroring, so a long stall is not loss)"
  for n in $ALL; do note "spdk-tgt restarts post on $n: $(spdk_restarts "$n")"; done
  note "raid state post: $(raid_summary "$RAID_HOST" | head -2)"

  EXPECT_RESCHEDULE=none READY_TIMEOUT=120 \
    NOTES="all-at-once ${N_ALL}-node csi-node kill incl raid host; degraded_direct=${DD_SEEN:-none}; acked_tail_risk=${RISK_SEEN:-none}; torn=${TORN:-NEVER}s; base_back=${BACK:-NEVER}s; rebuild=$([ -n "$TORN" ] && [ -n "$BACK" ] && echo $((BACK - TORN)) || echo NA)s; worst_ack_age=${WORST}s; raid=$(raid_summary "$RAID_HOST" | head -1)" verify
  ;;

*) fail "unknown drill '$DRILL' (phase-1 regression subset: PHASE_LABEL=2 ./drills/phase1.sh <id>)" ;;
esac
