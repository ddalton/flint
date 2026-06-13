#!/usr/bin/env bash
# Tier-2 phase 7a spike runner — manual skip_rebuild hot rejoin.
# See docs/tier2-evaluation-2026-06-12.md for the four deliverables.
#
# Drives the §7 hot-rejoin sequence by hand against a degraded 2-replica
# volume, with the cutover orchestrator deliberately out of the picture
# (plain RWO volume, no rejoin-bounce annotation — the exact
# restart-intolerant case Tier 2 exists for):
#
#   quiesce(lease) -> final snapshot E_f on survivor -> export E_f ->
#   esnap-clone head on R_dst -> export head -> attach on consumer ->
#   bdev_raid_add_base_bdev --skip-rebuild -> unquiesce
#
# Usage:
#   tier2-spike.sh plan <pv-name>     # discover topology, print the plan
#   tier2-spike.sh rejoin <pv-name>   # execute the window (timed)
#   tier2-spike.sh lease-drill <pv>   # quiesce, never renew, measure stall
#
# Requires: KUBECONFIG to the test cluster; the patched spdk-tgt image
# rolled to all nodes; jq.
set -euo pipefail

NS=flint-system
LEASE_MS=${LEASE_MS:-10000}

die() { echo "FATAL: $*" >&2; exit 1; }
ts() { python3 -c 'import time; print(f"{time.time():.3f}")'; }

# rpc <node> <args...> — run rpc.py inside the node's spdk-tgt container
rpc() {
	local node=$1; shift
	local pod
	pod=$(kubectl get pods -n "$NS" -o wide --no-headers | awk -v n="$node" '$7==n && /flint-csi-node/ {print $1}')
	[ -n "$pod" ] || die "no flint-csi-node pod on $node"
	kubectl exec -n "$NS" "$pod" -c spdk-tgt -- python3 /usr/local/scripts/rpc.py "$@"
}

# Topology discovery from the PV's replica-sync record.
discover() {
	local pv=$1
	RECORD=$(kubectl get pv "$pv" -o jsonpath='{.metadata.annotations.flint\.csi\.storage\.io/replica-sync-state}')
	[ -n "$RECORD" ] || die "no replica-sync record on $pv"

	CONSUMER_NODE=$(kubectl get volumeattachment -o json | jq -r \
		--arg pv "$pv" '.items[] | select(.spec.source.persistentVolumeName==$pv) | .spec.nodeName' | head -1)
	[ -n "$CONSUMER_NODE" ] || die "$pv has no attachment (need a running consumer)"

	SURVIVOR_NODE=$(echo "$RECORD" | jq -r '.replicas[] | select(.sync_state=="in_sync") | .node_name' | head -1)
	STALE_NODE=$(echo "$RECORD" | jq -r '.replicas[] | select(.sync_state!="in_sync") | .node_name' | head -1)
	STALE_STATE=$(echo "$RECORD" | jq -r '.replicas[] | select(.sync_state!="in_sync") | .sync_state' | head -1)
	STALE_UUID=$(echo "$RECORD" | jq -r '.replicas[] | select(.sync_state!="in_sync") | .lvol_uuid' | head -1)
	[ -n "$SURVIVOR_NODE" ] || die "no in_sync replica"
	[ -n "$STALE_NODE" ] || die "no stale/standby replica to rejoin"

	RAID="raid_$pv"
	LVS_SURV=$(rpc "$SURVIVOR_NODE" bdev_lvol_get_lvstores | jq -r '.[0].name')
	LVS_DST=$(rpc "$STALE_NODE" bdev_lvol_get_lvstores | jq -r '.[0].name')
	SURV_IP=$(kubectl get nodes "$SURVIVOR_NODE" -o jsonpath='{.status.addresses[?(@.type=="InternalIP")].address}')
	DST_IP=$(kubectl get nodes "$STALE_NODE" -o jsonpath='{.status.addresses[?(@.type=="InternalIP")].address}')

	echo "volume:        $pv"
	echo "raid:          $RAID (consumer: $CONSUMER_NODE)"
	echo "survivor:      $SURVIVOR_NODE ($SURV_IP) lvs=$LVS_SURV"
	echo "rejoin target: $STALE_NODE ($DST_IP) lvs=$LVS_DST state=$STALE_STATE uuid=$STALE_UUID"
}

plan() { discover "$1"; }

rejoin() {
	local pv=$1
	discover "$pv"
	local ef="spike-ef-$$" head="spike-head-$$"
	local nqn_ef="nqn.2016-06.io.spdk:spike-ef-$pv" nqn_head="nqn.2016-06.io.spdk:spike-head-$pv"

	echo "--- window opens ---"
	T0=$(ts)
	# 1. leased quiesce on the consumer's raid
	rpc "$CONSUMER_NODE" bdev_raid_quiesce "$RAID" --lease-ms "$LEASE_MS" >/dev/null
	T1=$(ts)

	# 2. final snapshot of the survivor leg (static under the quiesce)
	rpc "$SURVIVOR_NODE" bdev_lvol_snapshot "$LVS_SURV/$pv" "$ef" >/dev/null
	T2=$(ts)

	# 3. expose E_f over NVMe-oF; attach on the rejoin-target node
	rpc "$SURVIVOR_NODE" nvmf_create_subsystem "$nqn_ef" -a >/dev/null
	rpc "$SURVIVOR_NODE" nvmf_subsystem_add_ns "$nqn_ef" "$LVS_SURV/$ef" >/dev/null
	rpc "$SURVIVOR_NODE" nvmf_subsystem_add_listener "$nqn_ef" -t tcp -a "$SURV_IP" -s 4420 >/dev/null
	rpc "$STALE_NODE" bdev_nvme_attach_controller -b "spike_ef" -t tcp -a "$SURV_IP" -s 4420 -f ipv4 -n "$nqn_ef" >/dev/null
	T3=$(ts)

	# 4. esnap-clone head on R_dst (thin, instantly E_f-consistent)
	rpc "$STALE_NODE" bdev_lvol_clone_bdev "spike_efn1" "$LVS_DST" "$head" >/dev/null
	T4=$(ts)

	# 5. expose the new head; attach on the consumer
	rpc "$STALE_NODE" nvmf_create_subsystem "$nqn_head" -a >/dev/null
	rpc "$STALE_NODE" nvmf_subsystem_add_ns "$nqn_head" "$LVS_DST/$head" >/dev/null
	rpc "$STALE_NODE" nvmf_subsystem_add_listener "$nqn_head" -t tcp -a "$DST_IP" -s 4420 >/dev/null
	rpc "$CONSUMER_NODE" bdev_nvme_attach_controller -b "spike_head" -t tcp -a "$DST_IP" -s 4420 -f ipv4 -n "$nqn_head" >/dev/null
	T5=$(ts)

	# 6. the patched add
	rpc "$CONSUMER_NODE" bdev_raid_add_base_bdev "$RAID" "spike_headn1" --skip-rebuild >/dev/null
	T6=$(ts)

	# 7. release the lease
	rpc "$CONSUMER_NODE" bdev_raid_unquiesce "$RAID" >/dev/null
	T7=$(ts)
	echo "--- window closed ---"

	python3 - "$T0" "$T1" "$T2" "$T3" "$T4" "$T5" "$T6" "$T7" <<-'PYEOF'
	import sys
	t = [float(x) for x in sys.argv[1:]]
	steps = ["quiesce", "snapshot E_f", "export+attach E_f", "esnap clone head",
	         "export+attach head", "add --skip-rebuild", "unquiesce"]
	for name, a, b in zip(steps, t, t[1:]):
	    print(f"  {name:24s} {(b-a)*1000:8.1f} ms")
	print(f"  {'TOTAL WINDOW':24s} {(t[-1]-t[0])*1000:8.1f} ms")
	PYEOF

	rpc "$CONSUMER_NODE" bdev_raid_get_bdevs online | jq -r --arg r "$RAID" \
		'.[] | select(.name==$r) | .base_bdevs_list[] | "\(.name) configured=\(.is_configured)"'
	echo "NOTE: cleanup (subsystems/snapshots) is manual; record state NOT flipped (spike ships dark)."
}

lease_drill() {
	local pv=$1
	discover "$pv"
	echo "Quiescing with a ${LEASE_MS}ms lease and never renewing — watch the writer stall then resume."
	T0=$(ts)
	rpc "$CONSUMER_NODE" bdev_raid_quiesce "$RAID" --lease-ms "$LEASE_MS" >/dev/null
	echo "quiesced at $T0; lease expires in ${LEASE_MS}ms; NOT renewing, NOT unquiescing."
	echo "Verify in spdk-tgt logs on $CONSUMER_NODE: 'Quiesce lease ... expired without renewal'."
}

cmd=${1:-}; shift || true
case "$cmd" in
	plan)        plan "$@";;
	rejoin)      rejoin "$@";;
	lease-drill) lease_drill "$@";;
	*) die "usage: $0 plan|rejoin|lease-drill <pv-name>";;
esac
