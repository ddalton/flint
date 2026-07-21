# Shared helpers for the flint CSI attach/detach chaos campaign.
# Each drill sources this. Patterns ported from tests/k8s/pnfs-drills/lib.sh
# (step/fail, wait_pod_replaced, SSM restore) and
# scripts/cleanup-stuck-volumeattachments.sh (VA predicates).
#
# Env:
#   KUBECONFIG   required
#   NS           workload namespace          (flint-chaos)
#   DRIVER_NS    flint chart namespace       (flint-system)
#   AWS_REGION   region for SSM/EC2 drills   (us-west-1)
#   AWS_PROFILE  should be rolesanywhere for the ☠ drills

NS=${NS:-flint-chaos}
DRIVER_NS=${DRIVER_NS:-flint-system}
AWS_REGION=${AWS_REGION:-us-west-1}
CHAOS_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
RESULTS=${RESULTS:-$CHAOS_DIR/results.csv}
ARTIFACTS=${ARTIFACTS:-$CHAOS_DIR/artifacts}

step() { printf '\n▶ %s\n' "$*"; }
ok()   { printf '  ✓ %s\n' "$*"; }
note() { printf '  · %s\n' "$*"; }
fail() { printf '\n✗ %s\n' "$*" >&2; exit 1; }

epoch()   { date +%s; }
rfc3339() { date -u -r "$1" +%Y-%m-%dT%H:%M:%SZ; }  # BSD date (macOS)

need_env() {
  [ -n "${KUBECONFIG:-}" ] || fail "KUBECONFIG not set"
  kubectl get nodes >/dev/null 2>&1 || fail "cluster unreachable"
}

# ---- workload accessors ---------------------------------------------------

PG=pg-0
pg_node()     { kubectl get pod -n "$NS" $PG -o jsonpath='{.spec.nodeName}' 2>/dev/null; }
pg_uid()      { kubectl get pod -n "$NS" $PG -o jsonpath='{.metadata.uid}' 2>/dev/null; }
pg_restarts() { kubectl get pod -n "$NS" $PG -o jsonpath='{.status.containerStatuses[?(@.name=="postgres")].restartCount}' 2>/dev/null; }
pg_pv()       { kubectl get pvc -n "$NS" data-pg-0 -o jsonpath='{.spec.volumeName}' 2>/dev/null; }
load_pod()    { kubectl get pod -n "$NS" -l app=pg-load --field-selector status.phase=Running -o jsonpath='{.items[0].metadata.name}' 2>/dev/null; }

controller_pod() { kubectl get pod -n "$DRIVER_NS" -l app=flint-csi-controller --field-selector status.phase=Running -o jsonpath='{.items[0].metadata.name}' 2>/dev/null; }
csi_node_pod() { # <node>
  kubectl get pod -n "$DRIVER_NS" -l app=flint-csi-node \
    --field-selector "spec.nodeName=$1" -o jsonpath='{.items[0].metadata.name}' 2>/dev/null
}
worker_nodes() {
  kubectl get nodes -l '!node-role.kubernetes.io/control-plane' \
    -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}'
}

harness_healthy() {
  kubectl wait --for=condition=Ready pod/$PG -n "$NS" --timeout=30s >/dev/null 2>&1 \
    || fail "pg-0 not Ready — deploy/reset the harness first"
  [ -n "$(load_pod)" ] || fail "pg-load not Running"
  local last now
  last=$(timeout 15 kubectl exec -n "$NS" "$(load_pod)" -- sh -c 'tail -1 /acked/acked.log 2>/dev/null' | awk '{print $2}')
  now=$(epoch)
  [ -n "$last" ] && [ $(( now - last )) -lt 30 ] || fail "ledger not acking (last ack ${last:-none}, now $now) — is the load running?"
  ok "harness healthy (pg-0 Ready, ledger acking)"
}

# Ensure load has been running against a healthy DB for >=N seconds of acks.
warm_load() { # [secs]
  local want=${1:-60} t0
  t0=$(epoch)
  note "warming load ${want}s"
  sleep "$want"
  harness_healthy
}

# ---- kill-vector helpers --------------------------------------------------

# kubelet control via a privileged nsenter pod (kubelet must be alive to
# START it, so this only works for stop; restore goes via SSM).
kubelet_stop() { # <node>
  local n=$1
  kubectl run "kubelet-kill-$$" --image=busybox:1.36 --restart=Never \
    --overrides="{\"spec\":{\"nodeName\":\"${n}\",\"hostPID\":true,\"containers\":[{\"name\":\"k\",\"image\":\"busybox:1.36\",\"command\":[\"nsenter\",\"-t\",\"1\",\"-m\",\"-u\",\"-i\",\"-n\",\"--\",\"sh\",\"-c\",\"systemctl stop kubelet && echo STOPPED\"],\"securityContext\":{\"privileged\":true}}]}}" >/dev/null
  sleep 8
  kubectl delete pod "kubelet-kill-$$" --wait=false >/dev/null 2>&1
}

instance_id_for_node() { # <node> — providerID, falling back to InternalIP match
  local n=$1 id ip
  id=$(kubectl get node "$n" -o jsonpath='{.spec.providerID}' 2>/dev/null | sed 's|.*/||')
  if [ -z "$id" ] && command -v aws >/dev/null; then
    ip=$(kubectl get node "$n" -o jsonpath='{.status.addresses[?(@.type=="InternalIP")].address}')
    id=$(aws ec2 describe-instances --region "$AWS_REGION" \
      --filters "Name=private-ip-address,Values=${ip}" "Name=instance-state-name,Values=running" \
      --query "Reservations[].Instances[].InstanceId" --output text 2>/dev/null)
  fi
  echo "$id"
}

ssm_run() { # <instance-id> <command...>
  local iid=$1; shift
  aws ssm send-command --region "$AWS_REGION" --instance-ids "$iid" \
    --document-name AWS-RunShellScript --parameters commands="$*" \
    --query 'Command.CommandId' --output text
}

kubelet_start_ssm() { # <instance-id>
  ssm_run "$1" "systemctl start kubelet" >/dev/null \
    && note "kubelet start sent via SSM to $1"
}

taint_oos()   { kubectl taint nodes "$1" node.kubernetes.io/out-of-service=nodeshutdown:NoExecute >/dev/null; }
untaint_oos() { kubectl taint nodes "$1" node.kubernetes.io/out-of-service- >/dev/null 2>&1; }

wait_node_notready() { # <node> [budget_s]
  local n=$1 budget=${2:-180} st i
  for i in $(seq 1 $(( budget / 5 ))); do
    st=$(kubectl get node "$n" -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}' 2>/dev/null)
    [ "$st" != "True" ] && return 0
    sleep 5
  done
  return 1
}

wait_node_ready() { # <node> [budget_s]
  local n=$1 budget=${2:-300} st i
  for i in $(seq 1 $(( budget / 5 ))); do
    st=$(kubectl get node "$n" -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}' 2>/dev/null)
    [ "$st" = "True" ] && return 0
    sleep 5
  done
  return 1
}

# From pnfs-drills/lib.sh: `kubectl wait` right after a delete can match the
# OLD Terminating pod — wait for the UID to change first, then readiness.
wait_pod_replaced() { # <ns> <pod> <old_uid> <budget_s>
  local ns=$1 pod=$2 old_uid=$3 budget=$4 i uid ready
  for i in $(seq 1 $(( budget / 5 ))); do
    uid=$(kubectl get pod -n "$ns" "$pod" -o jsonpath='{.metadata.uid}' 2>/dev/null || true)
    ready=$(kubectl get pod -n "$ns" "$pod" -o jsonpath='{.status.containerStatuses[0].ready}' 2>/dev/null || true)
    [ -n "$uid" ] && [ "$uid" != "$old_uid" ] && [ "$ready" = "true" ] && return 0
    sleep 5
  done
  return 1
}

# ---- observation helpers --------------------------------------------------

# Max gap (s) between consecutive ledger acks since t0. The honest stall
# metric: resolution ≈ insert cadence (~0.2s) + psql connect time.
max_stall_since() { # <t0>
  local t0=$1
  kubectl exec -n "$NS" "$(load_pod)" -- sh -c "awk -v t0=$t0 '\$2>=t0' /acked/acked.log" 2>/dev/null \
    | awk 'NR>1 { gap=$2-prev; if (gap>max) max=gap } { prev=$2 } END { print max+0 }'
}

va_for_pv() { # <pv> — name of the VolumeAttachment for a PV ("" if none)
  kubectl get volumeattachments -o json \
    | jq -r --arg pv "$1" '.items[] | select(.spec.source.persistentVolumeName==$pv) | .metadata.name'
}

va_node_for_pv() { # <pv>
  kubectl get volumeattachments -o json \
    | jq -r --arg pv "$1" '.items[] | select(.spec.source.persistentVolumeName==$pv) | .spec.nodeName'
}

# Stale VAs: carrying a deletionTimestamp older than <age>s, or attached to a
# PV that no longer exists (predicates from cleanup-stuck-volumeattachments.sh).
stale_vas() { # [age_s]
  local age=${1:-120} now
  now=$(epoch)
  kubectl get volumeattachments -o json | jq -r --argjson now "$now" --argjson age "$age" '
    .items[]
    | select(.metadata.deletionTimestamp != null)
    | select((($now - (.metadata.deletionTimestamp | fromdateiso8601))) > $age)
    | .metadata.name'
}

nvme_subsys() { # <node> — nvme list-subsys from the csi driver container
  local pod
  pod=$(csi_node_pod "$1")
  [ -n "$pod" ] || { echo "NO-CSI-NODE-POD"; return; }
  kubectl exec -n "$DRIVER_NS" "$pod" -c flint-csi-driver -- nvme list-subsys 2>/dev/null
}

# Flint volume NVMe controllers on a node as "pv<TAB>state" lines. NQN form:
# nqn.2024-11.com.flint:volume:<pv>. Only flint volume subsystems (skips the
# node's EBS/instance-store pcie controllers).
flint_sessions() { # <node>
  nvme_subsys "$1" | awk '
    /com\.flint:volume:/ { split($0, a, "com.flint:volume:"); pv=a[2]; next }
    pv && /[[:space:]](live|connecting|resetting|deleting)$/ { print pv "\t" $NF; pv="" }'
}

# Set of live flint PV names (one per line).
live_flint_pvs() {
  kubectl get pv -o json | jq -r '.items[]
    | select(.spec.csi.driver=="flint.csi.storage.io") | .metadata.name'
}

# Orphaned flint sessions across the fleet: a controller whose PV no longer
# exists (leak — target lvol gone, initiator still reconnecting). Prints
# "node pv state" lines. Empty = clean.
orphan_flint_sessions() {
  local live n pv st
  live=$(live_flint_pvs)
  for n in $(worker_nodes); do
    while IFS=$'\t' read -r pv st; do
      [ -n "$pv" ] || continue
      echo "$live" | grep -qx "$pv" || echo "$n $pv $st"
    done < <(flint_sessions "$n")
  done
}

# ---- ublk-backend analogs (BLOCK_DEVICE_BACKEND=ublk clusters) --------------
# In ublk mode the kernel-facing device is /dev/ublkb<id> served by spdk-tgt;
# there are NO kernel nvme sessions (the remote leg lives inside SPDK as a
# bdev_nvme initiator controller). Liveness = the PV's ublk disk is served on
# the pod's node; leak = a served disk / initiator controller whose PV is gone.
backend_mode() { # ublk | nvmeof — from the csi-node DS env, cached per run
  if [ -z "${_BACKEND_MODE:-}" ]; then
    _BACKEND_MODE=$(kubectl get ds -n "$DRIVER_NS" -o json 2>/dev/null | jq -r '
      [.items[].spec.template.spec.containers[]? | select(.name=="flint-csi-driver")
       | .env[]? | select(.name=="BLOCK_DEVICE_BACKEND") | .value][0] // "nvmeof"')
    _BACKEND_MODE=${_BACKEND_MODE:-nvmeof}
  fi
  echo "$_BACKEND_MODE"
}

agent_spdk_rpc() { # <node> <json-body> — SPDK RPC via the node agent HTTP proxy
  local pod; pod=$(csi_node_pod "$1")
  [ -n "$pod" ] || return 1
  kubectl exec -n "$DRIVER_NS" "$pod" -c flint-csi-driver -- \
    curl -s -m 10 -X POST http://127.0.0.1:9081/api/spdk/rpc \
    -H 'Content-Type: application/json' -d "$2" 2>/dev/null
}

flint_ublk_disks() { # <node> — "id<TAB>bdev" lines of ublk disks SPDK serves
  agent_spdk_rpc "$1" '{"method":"ublk_get_disks"}' \
    | jq -r '.result[]? | "\(.id // .ublk_id)\t\(.bdev_name)"' 2>/dev/null
}

pv_ublk_id() { # <pv> — stage-time id annotation (authoritative)
  kubectl get pv "$1" -o jsonpath='{.metadata.annotations.flint\.io/ublk-id}' 2>/dev/null
}

# SPDK-initiator controllers for flint volumes on a node, as bare pv names
# (controller name = nvme_<nqn mangled>, nqn tail = ...com_flint_volume_<pv>).
flint_spdk_controllers() { # <node>
  agent_spdk_rpc "$1" '{"method":"bdev_nvme_get_controllers"}' \
    | jq -r '.result[]? | .name' 2>/dev/null | sed -n 's/.*com_flint_volume_//p'
}

# RWX topology: the block volume is staged on the flint-nfs pod's node (no
# ublk-id PV annotation — that's written by the RWO NodeStage). The pod's own
# /proc/mounts names its ublk device, which is the authoritative live id.
is_rwx() { # does the harness PVC use ReadWriteMany?
  kubectl get pvc -n "$NS" data-pg-0 -o jsonpath='{.spec.accessModes[0]}' 2>/dev/null \
    | grep -q ReadWriteMany
}
rwx_nfs_pod_for_pv() { # <pv> — flint-nfs pod name ("" if none)
  local h
  h=$(kubectl get pv "$1" -o jsonpath='{.spec.csi.volumeHandle}' 2>/dev/null)
  kubectl get pod -n "$DRIVER_NS" "flint-nfs-$h" -o jsonpath='{.metadata.name}' 2>/dev/null
}
rwx_nfs_node_for_pv() { # <pv>
  local p; p=$(rwx_nfs_pod_for_pv "$1")
  [ -n "$p" ] && kubectl get pod -n "$DRIVER_NS" "$p" -o jsonpath='{.spec.nodeName}' 2>/dev/null
}
rwx_ublk_id_for_pv() { # <pv> — ublk id backing the nfs pod's /mnt/volume
  local p; p=$(rwx_nfs_pod_for_pv "$1")
  [ -n "$p" ] || return 0
  kubectl exec -n "$DRIVER_NS" "$p" -- sh -c \
    'awk "\$2==\"/mnt/volume\"{print \$1}" /proc/mounts' 2>/dev/null \
    | grep -oE 'ublkb[0-9]+' | grep -oE '[0-9]+' | head -1
}
# "node id" pairs that are live because an nfs pod serves that volume there.
rwx_live_ublk_pairs() {
  local pv node id
  for pv in $(live_flint_pvs); do
    node=$(rwx_nfs_node_for_pv "$pv"); [ -n "$node" ] || continue
    id=$(rwx_ublk_id_for_pv "$pv"); [ -n "$id" ] || continue
    echo "$node $id"
  done
}

# Orphans in ublk mode: a served ublk disk whose id maps to no live PV, or an
# SPDK initiator controller whose PV is gone. "node kind detail" lines.
orphan_ublk_paths() {
  local live live_ids rwx_pairs n id bdev pv
  live=$(live_flint_pvs)
  live_ids=$(for pv in $live; do pv_ublk_id "$pv"; done | grep . || true)
  rwx_pairs=$(rwx_live_ublk_pairs)
  for n in $(worker_nodes); do
    while IFS=$'\t' read -r id bdev; do
      [ -n "$id" ] || continue
      echo "$live_ids" | grep -qx "$id" && continue
      # RWX: the disk backing a live volume's nfs pod on THIS node is live.
      echo "$rwx_pairs" | grep -qx "$n $id" && continue
      echo "$n ublk $id($bdev)"
    done < <(flint_ublk_disks "$n")
    while read -r pv; do
      [ -n "$pv" ] || continue
      # r2+ initiator controllers carry a _<replica-idx> suffix the PV
      # name doesn't have — strip it before the live match (2u/2.1 false
      # "nvme-leak" counted the live RAID leg as an orphan).
      base=${pv%_[0-9]}
      echo "$live" | grep -qx "$base" || echo "$n ctrl $pv"
    done < <(flint_spdk_controllers "$n")
  done
}

# Backend-dispatching orphan check — use this in drivers and verify.
orphan_data_paths() {
  if [ "$(backend_mode)" = "ublk" ]; then orphan_ublk_paths; else orphan_flint_sessions; fi
}

globalmounts() { # <node> — count of staged flint volumes on the node
  local pod
  pod=$(csi_node_pod "$1")
  [ -n "$pod" ] || { echo "-1"; return; }
  kubectl exec -n "$DRIVER_NS" "$pod" -c flint-csi-driver -- \
    sh -c 'ls -d /var/lib/kubelet/plugins/kubernetes.io/csi/*/*/globalmount 2>/dev/null | wc -l' | tr -d ' '
}

# Pod-mount orphans: kubelet pod-dir mounts whose pod UID is no longer live.
orphan_pod_mounts() { # <node>
  local pod live
  pod=$(csi_node_pod "$1")
  [ -n "$pod" ] || return 0
  live=$(kubectl get pods -A -o jsonpath='{range .items[*]}{.metadata.uid}{"\n"}{end}')
  kubectl exec -n "$DRIVER_NS" "$pod" -c flint-csi-driver -- \
    sh -c "grep -o '/var/lib/kubelet/pods/[0-9a-f-]*' /proc/mounts | sort -u" 2>/dev/null \
    | sed 's|.*/pods/||' \
    | while read -r uid; do echo "$live" | grep -q "^$uid$" || echo "$uid"; done
}

csv_append() { # phase,drill,... (writes header on first use)
  [ -f "$RESULTS" ] || echo "date,phase,drill,t_ready_s,stall_s,restarts_delta,pre_node,post_node,va_ok,nvme_ok,mounts_ok,db_verdict,logscan,verdict,notes" > "$RESULTS"
  echo "$*" >> "$RESULTS"
}
