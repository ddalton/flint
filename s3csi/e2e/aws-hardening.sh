#!/usr/bin/env bash
# s3.csi.chert.us on REAL nodes, second suite: the gaps the passthrough
# suite (aws-passthrough.sh) left — lean under a lost node and a
# rebooted node, a broker outage that outlives the key, one node at
# scale through a plugin roll, and lean at manifest scale on real S3.
#
# Runs AFTER `run-s3csi.sh setup` (STORE=s3 NODE_EXEC=nodesh) on an EC2
# cluster with TWO workers against one real bucket. The helpers are the
# single-cluster drill's own, evaluated at run time.
#
#   L1  lean, REAL node reboot: the graceful shutdown DRAINS the
#       unpublished write, the tree survives, the syncer is re-launched
#       into the same worker, the holder self-recognises without
#       rotating, and it publishes again
#   L3  a broker outage that OUTLIVES the key: the tenant is told, the
#       cached key is kept (reads continue on it — with an STS backend
#       the upstream would refuse at expiry, and the leg says so), and
#       rotation resumes once the broker returns
#   L4  one node at scale: SCALE tenants, each its own worker; a plugin
#       roll under them serves reads, restarts no worker, admits a new
#       tenant, and keeps rotating; every tenant is reclaimed
#   L5  lean at manifest scale: BIGN files seeded and published, checked
#       out cold ACROSS a plugin roll (S17 made real — the checkout does
#       not fit inside the roll), a publish that writes O(changed)
#       objects, and a takeover that rotates a BIGN-entry manifest
#   L6  a one-GiB workspace's ceiling is inodes, not bytes: the number
#       of files it admits, and the node's own disk is untouched
#   L7  the cost of preservation: an undrained tree is kept forever, and
#       nothing reclaims it — measured against the node's own headroom
#   L2  lean, HARD node loss: the node is POWERED OFF from under kubelet
#       (sysrq, no shutdown, no drain — a terminate is graceful and
#       drains, which is L1); the write since the last publish is lost,
#       the replacement waits out the unreleased lease, rotates, and
#       checks out the published set intact
#
# Env: as run-s3csi.sh (KUBECONFIG CTX BUCKET S3_REGION S3_KEY_FILE;
# STORE=s3 NODE_EXEC=nodesh forced) plus NODE2 (the second worker; L2
# terminates it), SCALE (default 120), BIGN (default 100000). The
# rolesanywhere profile terminates the instance in L2.
set -u
cd "$(dirname "$0")"
export STORE=s3 NODE_EXEC=nodesh
: "${NODE2:?}"
SCALE=${SCALE:-120}; BIGN=${BIGN:-100000}
REPO=$(cd ../.. && pwd)
eval "$(sed -n '/^CTX=\${CTX:-/,/^# ── setup \/ teardown/p' run-s3csi.sh | sed '$d')"
eval "$(sed -n '/^lobj()   {/,/^lmhas()  {/p' run-s3csi.sh)"
AWSR="env AWS_PROFILE=rolesanywhere AWS_REGION=$S3_REGION aws"
now() { date +%s; }
SKIPPED=0
skip() { SKIPPED=$((SKIPPED + 1)); echo "  SKIP: $1"; }
node_ready() { [ "$($K get node "$1" -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}' 2>/dev/null)" = "True" ]; }
restarts() { $K -n $NS get pod "$1" -o jsonpath='{.status.containerStatuses[0].restartCount}' 2>/dev/null; }
mounts_on_node() { onnode "grep -c 'plugins/s3.csi.chert.us/volumes' /proc/mounts"; }
onnode_at() { local n=$1; shift; "$REPO/scripts/nodesh.sh" "$n" "$*" 2>/dev/null; }
# The node shell is a pod, so anything that restarts kubelet takes it
# away for a while. An `onnode` that answers EMPTY is not a measurement
# of zero — the first run read an empty mount baseline that way and
# compared against it.
onnode_ready() { local n=$1 b=$2 i=0; while [ $i -lt "$b" ]; do [ "$(onnode_at "$n" 'echo ok')" = "ok" ] && return 0; sleep 5; i=$((i + 5)); done; return 1; }
# Wait for a NEW Ready plugin pod on the node after `delete pod`. NOT
# `kubectl rollout status`: a bare pod delete leaves the DaemonSet's
# generation unchanged, so its status subresource still describes the
# pre-delete world and rollout status can answer "successfully rolled
# out" before the controller has noticed (it did, on the first run).
# And `plugin_pod` answers EMPTY mid-roll, so a bare `[ "$new" != "$old" ]`
# would pass on nothing at all.
wait_new_plugin() { # old-name budget -> new name on stdout, empty if none
    local old=$1 budget=$2 i=0 new=""
    while [ $i -lt "$budget" ]; do
        new=$(plugin_pod)
        if [ -n "$new" ] && [ "$new" != "$old" ] \
           && [ "$($K -n $SYS get pod "$new" -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}' 2>/dev/null)" = "True" ]; then
            printf '%s' "$new"; return 0
        fi
        sleep 5; i=$((i + 5))
    done
    return 1
}
hdcrs() { sed -e "s#__B__#$BUCKET#g" -e "s#__ENDPOINT__#$S3_ENDPOINT#g" -e "s#__BIGN__#$BIGN#g" hd-lean.yaml.tpl; }
leanpod() { sed -e "s#__NAME__#$1#g" -e "s#__CR__#$2#g" -e "s#__NODE__#$3#g" -e "s#__FILES__#$4#g" -e "s#__NONCE__#$5#g" hd-lean-pod.yaml.tpl | $K apply -f - >/dev/null; }
ptpod() { sed -e "s#__NAME__#$1#g" -e "s#__CR__#$2#g" -e "s#__NODE__#$3#g" -e "s#suite: pt#suite: hd-scale#" pt-pod.yaml.tpl; }
poddel() { $K -n $NS delete pod "$@" --ignore-not-found --wait=true --timeout=300s >/dev/null 2>&1; }
# The pod's log line for a seed phase, and its timestamp.
seedlog() { $K -n $NS logs "$1" 2>/dev/null | grep -E "^SEED $2" | tail -1; }
seedts()  { seedlog "$1" "$2" | awk '{print $NF}'; }
wait_seedlog() { local i=0; while [ $i -lt "$3" ] && [ -z "$(seedlog "$1" "$2")" ]; do sleep 5; i=$((i + 5)); done; [ -n "$(seedlog "$1" "$2")" ]; }
# Declare a publish from a tenant and wait for its ack (pod nonce secs).
declare_publish() {
    tsh "$1" "printf '{\"nonce\":\"$2\"}' > /workspace/.flint/publish.tmp && mv /workspace/.flint/publish.tmp /workspace/.flint/publish"
    local i=0; while [ $i -lt "$3" ] && ! inpod "$1" "grep -q $2 /workspace/.flint/publish.ack 2>/dev/null && echo yes" | grep -q yes; do sleep 2; i=$((i + 2)); done
    inpod "$1" "grep -q $2 /workspace/.flint/publish.ack 2>/dev/null && echo yes" | grep -q yes
}
files_in() { inpod "$1" "find /workspace/src -type f 2>/dev/null | wc -l" | tr -d ' '; }
worker_uid() { $K -n $WNS get pod "$1" -o jsonpath='{.metadata.uid}' 2>/dev/null; }
worker_rc()  { $K -n $WNS get pod "$1" -o jsonpath='{.status.containerStatuses[0].restartCount}' 2>/dev/null; }
syncer_alive() { [ "$($K -n $WNS exec "$1" -- /bin/sh -c 'ps | grep -c "[f]lint-sync run"' 2>/dev/null)" -ge 1 ]; }
creds_exp() { $K -n $WNS exec "$1" -- /bin/sh -c 'sed -n "s/.*\"Expiration\":\"\([^\"]*\)\".*/\1/p" /comm/creds.json 2>/dev/null' 2>/dev/null; }
epoch_of() { python3 -c 'import sys,datetime; print(int(datetime.datetime.fromisoformat(sys.argv[1].replace("Z","+00:00")).timestamp()))' "$1" 2>/dev/null; }
instance_of() { $AWSR ec2 describe-instances --filters "Name=private-ip-address,Values=$($K get node "$1" -o jsonpath='{.status.addresses[?(@.type=="InternalIP")].address}')" "Name=instance-state-name,Values=running" --query 'Reservations[0].Instances[0].InstanceId' --output text 2>/dev/null; }
echo "flint hardening on real nodes — cluster $CTX, node $NODE (+$NODE2), bucket $BUCKET, SCALE=$SCALE BIGN=$BIGN"
$K get csidriver s3.csi.chert.us >/dev/null 2>&1 || { echo "no s3.csi.chert.us — run run-s3csi.sh setup first"; exit 2; }
# Judged, not fired and forgotten: `apply` reports a rejected CR on
# stderr and exits non-zero, and the first run spent three legs failing
# on workspaces that were never created (spec.region does not exist on
# this CRD). A leg whose fixture is missing tests nothing.
if hdcrs | $K apply -f - >/dev/null 2>"$PWD/.hd-apply.err"; then
    n=$($K -n $NS get flintleanworkspaces proj-reboot proj-loss proj-big proj-tiny proj-keep proj-keep2 --no-headers 2>/dev/null | wc -l | tr -d ' ')
    [ "$n" = "6" ] && echo "  ok: PRECONDITION: 6 lean workspaces applied" || { echo "  BAD: only $n of 6 lean workspaces exist after the apply"; exit 2; }
else
    echo "  BAD: the lean workspaces were REFUSED: $(tr '\n' ' ' < "$PWD/.hd-apply.err" | cut -c1-300)"; rm -f "$PWD/.hd-apply.err"; exit 2
fi
rm -f "$PWD/.hd-apply.err"

# ── L1 lean, real reboot ──────────────────────────────────────────────
leg L1 "lean under a REAL reboot of $NODE: the tenant returns on a tree that survived, the syncer is re-launched into the SAME worker, the holder self-recognises without rotating, and publishes again"
poddel lean-reboot
mcx mc rm --recursive --force "m/$BUCKET/tenants/reboot/" >/dev/null 2>&1
leanpod lean-reboot proj-reboot "$NODE" 200 reboot-1
if wait_phase lean-reboot Running 400 && wait_seedlog lean-reboot PUBLISHED 300; then
    h0=$(lepoch tenants/reboot .holder_id); e0=$(lepoch tenants/reboot .epoch); s0=$(lmseq tenants/reboot); n0=$(lments tenants/reboot)
    [ -n "$h0" ] && [ "${n0:-0}" -ge 201 ] && lmhas tenants/reboot src/seeded && ok "PRECONDITION: seeded and published — holder $h0 at epoch $e0, seq $s0, $n0 entries" || bad "PRECONDITION: holder '$h0', entries '$n0' — not the seeded state"
    w0=$(worker_of lean-reboot); wu0=$(worker_uid "$w0"); rc0=$(restarts lean-reboot)
    tsh lean-reboot "echo late > /workspace/src/late.txt"
    [ -z "$(lobj tenants/reboot/files/src/late.txt)" ] && ok "late.txt is written in the tree and NOT in the bucket (unpublished, by design: floorSecs is an hour)" || bad "late.txt reached the bucket before any publish"
    # WHOSE shutdown is this? Attribute the drain rather than assume it.
    # kubelet's graceful node shutdown is what this chart's PriorityClass
    # ordering rides on, and it is OFF unless shutdownGracePeriod is set
    # — 0s on a stock kubeadm node. When it is zero the drain still runs,
    # because the syncer drains on SIGTERM whoever sends it and systemd
    # signals every process on the way down; what is lost is the
    # ordering and the budget.
    sgp=$(onnode "grep -E '^shutdownGracePeriod:' /var/lib/kubelet/config.yaml | awk '{print \$2}'")
    note "kubelet shutdownGracePeriod on $NODE is '${sgp:-unset}' — at 0s kubelet terminates no pods on shutdown, so a drain observed below is systemd's SIGTERM reaching the syncer, not kubelet's ordered teardown"
    onnode "nohup sh -c 'sleep 2; systemctl reboot' >/dev/null 2>&1 &" >/dev/null 2>&1
    # A graceful shutdown is NOT a loss. `systemctl reboot` runs
    # kubelet's graceful node shutdown, which terminates the pods, which
    # runs the worker's preStop, which DRAINS — so the unpublished write
    # is saved before the node goes down. The first run asserted the
    # opposite and read the drain's own publish as a rotation.
    i=0; while [ $i -lt 240 ] && [ -z "$(lobj tenants/reboot/files/src/late.txt)" ]; do sleep 5; i=$((i + 5)); done
    [ "$(lobj tenants/reboot/files/src/late.txt)" = "late" ] \
        && ok "the graceful shutdown DRAINED the workspace: the unpublished late.txt reached the bucket ${i}s after the reboot was issued, before the node went down" \
        || bad "late.txt never reached the bucket during the shutdown — the drain did not run"
    bs=$(lptr tenants/reboot .boundary_source); ed=$(lptr tenants/reboot .epoch)
    [ "$bs" = "drain" ] && [ "$ed" = "$e0" ] \
        && ok "the generation it wrote is marked boundary_source=drain at the SAME epoch ($ed): the drain saved it, no successor was involved" \
        || bad "the generation is boundary_source='$bs' at epoch '$ed' (holder epoch was $e0)"
    sdrain=$(lmseq tenants/reboot); ndrain=$(lments tenants/reboot)
    note "the drain published at seq $sdrain with $ndrain entries (before the reboot: seq $s0, $n0 entries)"
    i=0; while [ $i -lt 240 ] && node_ready "$NODE"; do sleep 5; i=$((i + 5)); done
    node_ready "$NODE" && bad "PRECONDITION: $NODE never went NotReady — no reboot happened" || ok "PRECONDITION: $NODE went NotReady (${i}s) — rebooting"
    i=0; while [ $i -lt 480 ] && ! node_ready "$NODE"; do sleep 10; i=$((i + 10)); done
    node_ready "$NODE" && ok "$NODE is Ready again (${i}s)" || bad "$NODE not Ready 480s after the reboot"
    i=0; while [ $i -lt 480 ] && { [ "$(restarts lean-reboot)" = "$rc0" ] || [ "$($K -n $NS get pod lean-reboot -o jsonpath='{.status.phase}')" != "Running" ]; }; do sleep 10; i=$((i + 10)); done
    [ "$(restarts lean-reboot)" != "$rc0" ] && ok "the tenant's container restarted on the rebooted node (${i}s) and is Running" || bad "the tenant never came back Running after the reboot: $(mount_events lean-reboot | tail -2 | tr '\n' ' ' | cut -c1-300)"
    [ "$(seedlog lean-reboot PRESENT)" != "" ] && ok "the restarted tenant found its tree already seeded (SEED PRESENT): the tree lived through the reboot" || bad "the restarted tenant did not find its seed: $(seedlog lean-reboot '' | tail -1)"
    [ "$(inpod lean-reboot cat /workspace/src/late.txt)" = "late" ] && ok "the UNPUBLISHED late.txt survived the reboot in the tree (on disk under the plugin dir, not in memory)" || bad "late.txt is gone after the reboot"
    i=0; while [ $i -lt 300 ] && { w1=$(worker_of lean-reboot); [ -z "$w1" ] || ! syncer_alive "$w1"; }; do sleep 5; i=$((i + 5)); done
    w1=$(worker_of lean-reboot)
    [ -n "$w1" ] && syncer_alive "$w1" && ok "flint-sync is running again inside the worker ${i}s after Ready (the launch was re-sent into the memory-backed comm dir the reboot emptied)" || bad "no live syncer in the worker $w1 after ${i}s"
    [ -n "$w1" ] && [ "$(worker_uid "$w1")" = "$wu0" ] && ok "the SAME worker pod ($w1, uid unchanged): re-launched, not recreated" || bad "the worker was recreated across the reboot ($w0 → $w1)"
    i=0; while [ $i -lt 120 ] && [ "$(lepoch tenants/reboot .epoch)" = "$e0" ]; do sleep 5; i=$((i + 5)); done
    h1=$(lepoch tenants/reboot .holder_id); e1=$(lepoch tenants/reboot .epoch); s1=$(lmseq tenants/reboot)
    [ "$h1" = "$h0" ] && ok "self-recognition: the holder id survived the reboot ($h1) — the incarnation persisted in the tree" || bad "the holder changed across a reboot of its own node ($h0 → $h1): a reboot was treated as a takeover"
    [ "$s1" = "$sdrain" ] && ok "no rotation across the relaunch (seq is still the drain's $sdrain): self-recognition is not a takeover, and at 100k entries a rotation is a multi-MB GET+PUT" || bad "the relaunch rotated the manifest ($sdrain → $s1)"
    [ "${e1:-0}" -gt "${e0:-0}" ] && ok "the epoch bumped ($e0 → $e1): a straggler from before the reboot cannot publish" || bad "the epoch did not move across the reboot ($e0 → $e1)"
    tsh lean-reboot "echo post > /workspace/src/post.txt"; t0=$(now)
    if declare_publish lean-reboot reboot-2 300; then
        ok "a publish declared AFTER the reboot was acked in $(( $(now) - t0 ))s"
        [ "$(lobj tenants/reboot/files/src/post.txt)" = "post" ] && lmhas tenants/reboot src/post.txt && ok "the post-reboot write is in the bucket and cited ($(lments tenants/reboot) entries at seq $(lmseq tenants/reboot))" || bad "the post-reboot write was not published"
    else
        bad "no ack for a publish declared after the reboot within 300s"
    fi
    stale=$(onnode 'cd /var/lib/kubelet/plugins/s3.csi.chert.us/volumes 2>/dev/null && for d in */; do [ -f "$d/state.json" ] || echo "$d"; done | wc -l')
    [ "${stale:-0}" = "0" ] && ok "no half-removed volume directory on $NODE after the reboot" || bad "$stale volume dir(s) without state after the reboot"
else
    bad "lean-reboot never seeded: phase $($K -n $NS get pod lean-reboot -o jsonpath='{.status.phase}' 2>/dev/null) — $(seedlog lean-reboot '' | tail -1) $(mount_events lean-reboot | tail -1 | cut -c1-200)"
fi
poddel lean-reboot

# ── L3 broker outage past the key lifetime ────────────────────────────
leg L3 "a broker outage that OUTLIVES the key (lifetime ${CREDS_LIFETIME}s): the tenant is told, the cached key is kept and reads continue on it, and rotation resumes when the broker returns"
if require_pod reader; then
    w=$(worker_of reader); exp0=$(creds_exp "$w"); ex0=$(epoch_of "$exp0"); c0=$(broker_issued)
    [ -n "$exp0" ] && [ "${ex0:-0}" -gt "$(now)" ] && ok "PRECONDITION: reader's worker holds a key expiring at $exp0 ($(( ex0 - $(now) ))s from now); broker issued so far: ${c0:-?}" || bad "PRECONDITION: no readable expiration in reader's creds.json ('$exp0')"
    [ "$(inpod_out reader cat /mnt/s3/shard-06.txt)" = "seeded-object-06" ] && ok "PRECONDITION: reader reads" || bad "PRECONDITION: reader cannot read before the outage"
    $K -n $SYS scale deploy/flint-s3-broker --replicas=0 >/dev/null 2>&1
    i=0; while [ $i -lt 120 ] && [ "$($K -n $SYS get deploy flint-s3-broker -o jsonpath='{.status.availableReplicas}' 2>/dev/null)" != "" ]; do sleep 2; i=$((i + 2)); done
    ok "the broker is scaled to zero (${i}s)"
    # Past expiry, by a margin: republish re-exchanges within three periods of expiry, so every refresh from here on FAILS.
    while [ "$(now)" -lt $((ex0 + 30)) ]; do sleep 5; done
    ev=$(mount_events reader | grep -c CredentialRefreshFailed || true)
    [ "${ev:-0}" -ge 1 ] && ok "CredentialRefreshFailed landed on the tenant pod ($ev event(s)) — the outage is named where the tenant looks" || bad "no CredentialRefreshFailed on reader $(( $(now) - ex0 ))s past the key's expiry"
    exp1=$(creds_exp "$w")
    [ "$exp1" = "$exp0" ] && ok "the cached key is KEPT through the outage (creds.json still there, same expiration, now $(( $(now) - ex0 ))s in the past)" || bad "creds.json changed or vanished during the outage ('$exp1')"
    got=$(inpod_out reader cat /mnt/s3/shard-06.txt)
    [ "$got" = "seeded-object-06" ] && ok "reads CONTINUE on the cached key past its expiry — the outage is not a data outage. Named: with the static backend the key is valid upstream; an STS backend's key would be refused upstream at this point and reads would fail" || bad "reads FAILED under the outage ('$got'): the cached key was not usable past expiry — an outage became a data outage"
    t1=$(now); $K -n $SYS scale deploy/flint-s3-broker --replicas=1 >/dev/null 2>&1
    $K -n $SYS rollout status deploy/flint-s3-broker --timeout=180s >/dev/null 2>&1
    # The counter lives in the broker PROCESS, so the outage reset it
    # (26 → 3 on the first run, and the leg read that as "no keys
    # issued"). Count from a baseline taken AFTER the restart; the reset
    # is itself worth recording — an outage costs the operator this
    # observability, it does not carry it across.
    c2=$(broker_issued); note "the issued counter restarted with the process (${c0} before the outage → ${c2} after): it counts this broker's life, not the cluster's"
    i=0; while [ $i -lt $((CREDS_LIFETIME * 3 + 120)) ] && [ "$(creds_exp "$w")" = "$exp0" ]; do sleep 5; i=$((i + 5)); done
    exp2=$(creds_exp "$w")
    [ -n "$exp2" ] && [ "$exp2" != "$exp0" ] && ok "rotation RESUMED $(( $(now) - t1 ))s after the broker returned (expiration $exp0 → $exp2)" || bad "the key was not refreshed within $((CREDS_LIFETIME * 3 + 120))s of the broker's return"
    c3=$(broker_issued); [ "${c3:-0}" -gt "${c2:-0}" ] && ok "the broker issued again after the recovery (${c2} → ${c3})" || bad "the broker's issued counter did not move after the recovery (${c2} → ${c3})"
    [ "$(inpod_out reader cat /mnt/s3/shard-06.txt)" = "seeded-object-06" ] && ok "reader reads after recovery" || bad "reader cannot read after recovery"
    ptpod hd-after-outage datasets "$NODE" | $K apply -f - >/dev/null
    wait_phase hd-after-outage Running 240 && [ "$(inpod_out hd-after-outage cat /mnt/s3/shard-05.txt)" = "seeded-object-05" ] && ok "a NEW tenant mounts and reads after the recovery (the exchange works again)" || bad "a new tenant failed to mount after the recovery: $(mount_events hd-after-outage | tail -1 | cut -c1-200)"
    poddel hd-after-outage
fi

# ── L4 one node at scale ──────────────────────────────────────────────
leg L4 "$SCALE tenants on $NODE, each its own worker: all mount and read; a plugin roll under them serves reads, restarts no worker, admits a new tenant and keeps rotating; every tenant is reclaimed"
onnode "grep -q '^maxPods:' /var/lib/kubelet/config.yaml && sed -i 's/^maxPods:.*/maxPods: 400/' /var/lib/kubelet/config.yaml || echo 'maxPods: 400' >> /var/lib/kubelet/config.yaml; systemctl restart kubelet" >/dev/null 2>&1
# The node shell is a pod on this node, and the kubelet restart takes it
# with it. Every `onnode` baseline below would come back EMPTY, which is
# not a measurement of zero — the first run compared the final mount
# count against an empty string.
"$REPO/scripts/nodesh-daemon.sh" up >/dev/null 2>&1
onnode_ready "$NODE" 300 && ok "PRECONDITION: the node shell answers again after the kubelet restart" || bad "PRECONDITION: no node shell after the kubelet restart — every on-node number below would be empty"
i=0; while [ $i -lt 180 ] && { ! node_ready "$NODE" || [ "$($K get node "$NODE" -o jsonpath='{.status.allocatable.pods}')" != "400" ]; }; do sleep 5; i=$((i + 5)); done
[ "$($K get node "$NODE" -o jsonpath='{.status.allocatable.pods}')" = "400" ] && ok "PRECONDITION: $NODE allocates 400 pods (kubeadm's default of 110 cannot hold $SCALE tenants plus their workers)" || bad "PRECONDITION: allocatable pods on $NODE is '$($K get node "$NODE" -o jsonpath='{.status.allocatable.pods}')', not 400"
m0=$(mounts_on_node); mem0=$(onnode "free -m | awk '/Mem:/{print \$3}'")
[ -n "$m0" ] && [ -n "$mem0" ] && ok "PRECONDITION: on-node baselines read ($m0 mounts, ${mem0} MiB used)" || bad "PRECONDITION: an on-node baseline came back empty (mounts '$m0', memory '$mem0') — the comparisons at the end would be against nothing"
c0=$(broker_issued); p0=$(plugin_pod); prc0=$($K -n $SYS get pod "$p0" -o jsonpath='{.status.containerStatuses[?(@.name=="flint-s3-csi-node")].restartCount}' 2>/dev/null)
wbase=$($K -n $WNS get pods --no-headers 2>/dev/null | wc -l | tr -d ' ')
t0=$(now); for i in $(seq 1 "$SCALE"); do ptpod "hd-s$i" datasets "$NODE"; echo "---"; done | $K apply -f - >/dev/null 2>&1
i=0; run=0; while [ $i -lt 1200 ]; do run=$($K -n $NS get pods -l suite=hd-scale -o json 2>/dev/null | jq '[.items[] | select(.status.phase=="Running")] | length'); [ "${run:-0}" -ge "$SCALE" ] && break; sleep 10; i=$((i + 10)); done
[ "${run:-0}" -ge "$SCALE" ] && ok "all $SCALE tenants Running in $(( $(now) - t0 ))s" || bad "$run of $SCALE tenants Running after ${i}s; sample: $($K -n $NS get events --field-selector reason=FailedMount -o jsonpath='{.items[-1:].message}' 2>/dev/null | cut -c1-200)"
wn=$($K -n $WNS get pods -o json 2>/dev/null | jq '[.items[] | select(.metadata.annotations["chert.us/tenant-pod"] | startswith("s3-tenants/hd-s")) | select(.status.phase=="Running")] | length')
[ "${wn:-0}" -ge "$SCALE" ] && ok "$wn Running workers for them (one each)" || bad "only $wn Running workers for $SCALE tenants"
m1=$(mounts_on_node); [ "$((m1 - m0))" -ge "$SCALE" ] && ok "$((m1 - m0)) new plugin-dir mounts on $NODE" || bad "only $((m1 - m0)) new mounts for $SCALE tenants"
rok=0; for i in 1 $((SCALE / 4)) $((SCALE / 2)) $((SCALE * 3 / 4)) $SCALE; do [ "$(inpod_out "hd-s$i" cat /mnt/s3/shard-01.txt)" = "seeded-object-01" ] && rok=$((rok + 1)); done
[ "$rok" = "5" ] && ok "5 of 5 sampled tenants (1, $((SCALE / 4)), $((SCALE / 2)), $((SCALE * 3 / 4)), $SCALE) read the seeded object" || bad "$rok of 5 sampled tenants read"
c1=$(broker_issued); [ "$(( ${c1:-0} - ${c0:-0} ))" -ge "$SCALE" ] && ok "the broker issued ≥$SCALE keys ($c0 → $c1)" || bad "the broker issued only $(( ${c1:-0} - ${c0:-0} )) keys for $SCALE tenants"
mem1=$(onnode "free -m | awk '/Mem:/{print \$3}'"); note "node memory used: ${mem0} MiB → ${mem1} MiB for $SCALE tenants + workers ($(( (mem1 - mem0) / SCALE )) MiB each); plugin RSS $(onnode "ps -o rss= -C flint-s3-csi-node 2>/dev/null | awk '{s+=\$1} END {print int(s/1024)}'") MiB"
[ "$($K -n $SYS get pod "$p0" -o jsonpath='{.status.containerStatuses[?(@.name=="flint-s3-csi-node")].restartCount}' 2>/dev/null)" = "$prc0" ] && ok "the plugin did not restart while publishing $SCALE volumes" || bad "the plugin restarted under $SCALE publishes"
# The roll, under load.
wrc0=$($K -n $WNS get pods -o json 2>/dev/null | jq '[.items[] | select(.metadata.annotations["chert.us/tenant-pod"] | startswith("s3-tenants/hd-s")) | .status.containerStatuses[0].restartCount] | add')
ex_a=$(creds_exp "$(worker_of hd-s1)")
$K -n $SYS delete pod "$p0" --wait=false >/dev/null 2>&1
rok=0; for i in 2 $((SCALE / 3)) $((SCALE - 1)); do [ "$(inpod_out "hd-s$i" cat /mnt/s3/shard-01.txt)" = "seeded-object-01" ] && rok=$((rok + 1)); done
[ "$rok" = "3" ] && ok "3 of 3 sampled tenants read WHILE the plugin pod is being replaced" || bad "$rok of 3 tenants read during the roll"
p1=$(wait_new_plugin "$p0" 300)
[ -n "$p1" ] && ok "the plugin rolled under $SCALE mounted volumes ($p0 → $p1, Ready)" || bad "no new Ready plugin pod on $NODE within 300s of deleting $p0"
wrc1=$($K -n $WNS get pods -o json 2>/dev/null | jq '[.items[] | select(.metadata.annotations["chert.us/tenant-pod"] | startswith("s3-tenants/hd-s")) | .status.containerStatuses[0].restartCount] | add')
[ "${wrc1:-x}" = "${wrc0:-y}" ] && ok "no worker restarted across the roll (restart sum $wrc1)" || bad "workers restarted across the roll (restart sum $wrc0 → $wrc1)"
m2=$(mounts_on_node); [ "$m2" = "$m1" ] && ok "mount count unchanged across the roll ($m2)" || bad "mount count moved across the roll ($m1 → $m2)"
ptpod "hd-s$((SCALE + 1))" datasets "$NODE" | $K apply -f - >/dev/null
wait_phase "hd-s$((SCALE + 1))" Running 240 && [ "$(inpod_out "hd-s$((SCALE + 1))" cat /mnt/s3/shard-02.txt)" = "seeded-object-02" ] && ok "tenant $((SCALE + 1)) mounts and reads after the roll (the new plugin adopted $SCALE volumes and still serves new ones)" || bad "a new tenant failed after the roll: $(mount_events "hd-s$((SCALE + 1))" | tail -1 | cut -c1-200)"
i=0; while [ $i -lt $((CREDS_LIFETIME * 3 + 120)) ] && [ "$(creds_exp "$(worker_of hd-s1)")" = "$ex_a" ]; do sleep 10; i=$((i + 10)); done
[ "$(creds_exp "$(worker_of hd-s1)")" != "$ex_a" ] && ok "keys still rotate after the roll (hd-s1's expiration advanced within ${i}s)" || bad "hd-s1's key did not rotate within ${i}s of the roll"
# Reclaim.
t0=$(now); $K -n $NS delete pods -l suite=hd-scale --wait=false >/dev/null 2>&1
i=0; while [ $i -lt 900 ] && [ "$($K -n $NS get pods -l suite=hd-scale --no-headers 2>/dev/null | wc -l | tr -d ' ')" != "0" ]; do sleep 10; i=$((i + 10)); done
[ "$($K -n $NS get pods -l suite=hd-scale --no-headers 2>/dev/null | wc -l | tr -d ' ')" = "0" ] && ok "all $((SCALE + 1)) tenants gone in $(( $(now) - t0 ))s" || bad "tenants remain 900s after the delete"
i=0; while [ $i -lt 300 ] && [ "$($K -n $WNS get pods --no-headers 2>/dev/null | wc -l | tr -d ' ')" != "$wbase" ]; do sleep 10; i=$((i + 10)); done
[ "$($K -n $WNS get pods --no-headers 2>/dev/null | wc -l | tr -d ' ')" = "$wbase" ] && ok "workers back to the baseline ($wbase) ${i}s later" || bad "$($K -n $WNS get pods --no-headers 2>/dev/null | wc -l | tr -d ' ') workers remain (baseline $wbase)"
m3=$(mounts_on_node); [ "${m3:-0}" -le "$m0" ] && ok "plugin-dir mounts back to the baseline ($m3)" || bad "$m3 plugin-dir mounts remain (baseline $m0)"

# ── L5 lean at manifest scale ─────────────────────────────────────────
leg L5 "lean at manifest scale: $BIGN files seeded and published; a cold checkout ACROSS a plugin roll (S17 made real); a publish that writes O(changed); a takeover that rotates a $BIGN-entry manifest"
poddel big-seeder big-agent big-agent2
mcx mc rm --recursive --force "m/$BUCKET/tenants/big/" >/dev/null 2>&1
leanpod big-seeder proj-big "$NODE" "$BIGN" big-1
if wait_phase big-seeder Running 400 && wait_seedlog big-seeder WRITTEN 1800; then
    tw=$(( $(seedts big-seeder WRITTEN) - $(seedts big-seeder START) )); note "writing $BIGN files in the workspace took ${tw}s"
    if wait_seedlog big-seeder '(PUBLISHED|NEVER)' 2400 && [ -n "$(seedlog big-seeder PUBLISHED)" ]; then
        tp=$(( $(seedts big-seeder PUBLISHED) - $(seedts big-seeder WRITTEN) ))
        ok "the publish of $BIGN files was acked in ${tp}s ($(( BIGN / (tp > 0 ? tp : 1) )) objects/s)"
        n0=$(lments tenants/big); [ "${n0:-0}" -ge "$((BIGN + 1))" ] && lmhas tenants/big src/d0/f42.txt && ok "the manifest cites $n0 entries" || bad "the manifest cites '$n0' entries, fewer than $((BIGN + 1))"
        nch=$(lptr tenants/big '.chunks | length'); note "the manifest is $nch chunk(s); pointer $(lobj tenants/big/.flint/lean/current | wc -c | tr -d ' ') B"
        poddel big-seeder
        # Cold checkout across a plugin roll: the S17 shape, now with a checkout that cannot finish inside the roll.
        leanpod big-agent proj-big "$NODE" "$BIGN" big-x
        t0=$(now); w=""; i=0; while [ $i -lt 1200 ] && [ -z "$w" ]; do w=$(worker_of_any big-agent); [ -z "$w" ] && { sleep 0.25; i=$((i + 1)); }; done
        if [ -n "$w" ]; then
            wu0=$(worker_uid "$w"); tree=$($K -n $WNS get pod "$w" -o jsonpath='{.spec.volumes[*].hostPath.path}' 2>/dev/null)
            sleep 20
            [ "$(onnode "test -f '$tree/.flint-sync/checkout-complete' && echo yes || echo no")" = "no" ] && ok "PRECONDITION: 20s in, the $BIGN-file checkout is still running (no marker) — the roll lands mid-checkout" || bad "PRECONDITION: the checkout already finished before the roll — S17 stays vacuous at this size"
            p0=$(plugin_pod); $K -n $SYS delete pod "$p0" --wait=false >/dev/null 2>&1
            p1=$(wait_new_plugin "$p0" 300)
            [ -n "$p1" ] && ok "the plugin rolled mid-checkout ($p0 → $p1, Ready)" || bad "no new Ready plugin pod within 300s of deleting $p0 mid-checkout"
            if wait_phase big-agent Running 2400; then
                tc=$(( $(now) - t0 )); ok "the cold checkout of $BIGN files completed and the tenant is Running ${tc}s after creation ($(( BIGN / tc )) files/s, plugin roll included)"
                [ "$(worker_uid "$w")" = "$wu0" ] && ok "the SAME worker finished it (uid unchanged): the checkout did not restart" || bad "the worker was replaced across the roll"
                fi=$(files_in big-agent); [ "$fi" = "$((BIGN + 1))" ] && ok "the tenant sees $fi files" || bad "the tenant sees '$fi' files, not $((BIGN + 1))"
                [ "$(inpod big-agent cat /workspace/src/d0/f42.txt)" = "unit 000042 of the seeded project" ] && ok "f42 carries the seeded bytes" || bad "f42 reads '$(inpod big-agent cat /workspace/src/d0/f42.txt)'"
                # O(changed).
                nf0=$(lcount tenants/big/files/); nc0=$(lcount tenants/big/.flint/lean/chunks/); s0=$(lmseq tenants/big)
                tsh big-agent "mkdir -p /workspace/src/new && for i in 1 2 3 4 5; do echo new-\$i > /workspace/src/new/n\$i.txt; done"
                t0=$(now)
                if declare_publish big-agent big-2 600; then
                    ta=$(( $(now) - t0 )); nf1=$(lcount tenants/big/files/); nc1=$(lcount tenants/big/.flint/lean/chunks/)
                    [ "$((nf1 - nf0))" = "5" ] && ok "a 5-file change published in ${ta}s and wrote 5 file objects — not $BIGN (O(changed))" || bad "the publish wrote $((nf1 - nf0)) file objects for a 5-file change"
                    note "chunk objects $nc0 → $nc1 (+$((nc1 - nc0))) for the 5-file publish; seq $s0 → $(lmseq tenants/big)"
                    [ "$(lments tenants/big)" = "$((n0 + 5))" ] && ok "the manifest cites $((n0 + 5)) entries (+5)" || bad "the manifest cites $(lments tenants/big), not $((n0 + 5))"
                else
                    bad "no ack for the 5-file publish within 600s"
                fi
                # Takeover at scale: freeze the holder, bring a successor.
                h0=$(lepoch tenants/big .holder_id); e0=$(lepoch tenants/big .epoch); s0=$(lmseq tenants/big); r0=$(lrenew tenants/big)
                sig_syncer "$w" STOP; sleep 40; r1=$(lrenew tenants/big)
                [ -n "$r0" ] && [ "$r1" = "$r0" ] && [ "$(lepoch tenants/big .released)" = "false" ] && ok "PRECONDITION: the holder is frozen (renewed_unix still $r1 across 40s) and unreleased — a successor must wait it out and rotate" || bad "PRECONDITION: the holder kept renewing or released ($r0 → $r1, released=$(lepoch tenants/big .released))"
                leanpod big-agent2 proj-big "$NODE" "$BIGN" big-y
                t0=$(now); te=""; ts=""; i=0
                while [ $i -lt 2400 ]; do
                    [ -z "$te" ] && [ "$(lepoch tenants/big .epoch)" != "$e0" ] && te=$(( $(now) - t0 ))
                    [ -z "$ts" ] && [ "$(lmseq tenants/big)" != "$s0" ] && ts=$(( $(now) - t0 ))
                    [ "$($K -n $NS get pod big-agent2 -o jsonpath='{.status.phase}' 2>/dev/null)" = "Running" ] && break
                    sleep 5; i=$((i + 5))
                done
                if [ "$($K -n $NS get pod big-agent2 -o jsonpath='{.status.phase}' 2>/dev/null)" = "Running" ]; then
                    ok "the successor superseded the frozen holder at +${te:-?}s, the $((n0 + 5))-entry manifest was rotated by +${ts:-?}s, and the successor was Running (checked out) at +$(( $(now) - t0 ))s"
                    [ "$(lepoch tenants/big .holder_id)" != "$h0" ] && [ "$(lmseq tenants/big)" != "$s0" ] && ok "a new holder ($(lepoch tenants/big .holder_id | cut -c1-13)…) at epoch $(lepoch tenants/big .epoch), seq $s0 → $(lmseq tenants/big)" || bad "holder/seq did not change ($h0/$s0 → $(lepoch tenants/big .holder_id)/$(lmseq tenants/big))"
                    [ "$(lments tenants/big)" = "$((n0 + 5))" ] && ok "no entry lost across the rotation ($((n0 + 5)))" || bad "entries after the rotation: $(lments tenants/big), not $((n0 + 5))"
                    [ "$(files_in big-agent2)" = "$((BIGN + 6))" ] && ok "the successor's tree has all $((BIGN + 6)) files" || bad "the successor's tree has $(files_in big-agent2) files"
                else
                    bad "the successor never reached Running in ${i}s (epoch moved at +${te:-never}s, seq at +${ts:-never}s): $(mount_events big-agent2 | tail -1 | cut -c1-200)"
                fi
                sig_syncer "$w" CONT
            else
                bad "big-agent never reached Running within 2400s of creation: $(mount_events big-agent | tail -2 | tr '\n' ' ' | cut -c1-300)"
            fi
        else
            bad "no worker appeared for big-agent"
        fi
    else
        bad "the $BIGN-file publish was not acked: $(seedlog big-seeder '' | tail -1) — syncer: $($K -n $WNS logs "$(worker_of big-seeder)" --tail=3 2>/dev/null | tr '\n' ' ' | cut -c1-300)"
    fi
else
    bad "big-seeder never seeded: $($K -n $NS get pod big-seeder -o jsonpath='{.status.phase}' 2>/dev/null) $(seedlog big-seeder '' | tail -1) $(mount_events big-seeder | tail -1 | cut -c1-200)"
fi
poddel big-seeder big-agent big-agent2

# ── L6 the one-GiB workspace's file ceiling ───────────────────────────
leg L6 "a one-GiB lean workspace: how many files it admits before ENOSPC (mkfs.ext4 at the default inode ratio ⇒ inodes, not bytes), and the node's root filesystem is untouched"
poddel tiny-agent
du0=$(onnode "df -m / | awk 'NR==2{print \$3}'")
leanpod tiny-agent proj-tiny "$NODE" 0 tiny-1
if wait_phase tiny-agent Running 400; then
    # NOTE the `true >` below, not `: >`. `:` is a POSIX *special*
    # builtin, and a redirection failure on a special builtin makes a
    # non-interactive shell exit outright — so the very ENOSPC this leg
    # exists to observe killed the probe before it could report, and the
    # leg read as "no output" (2026-09-04).
    got=$($K -n $NS exec tiny-agent -c agent -- /bin/sh -c 'i=0; mkdir -p /workspace/src/t || exit 3; while [ $i -lt 90000 ]; do true > /workspace/src/t/f$i 2>/dev/null || break; i=$((i + 1)); done; echo "FILES=$i"; df -m /workspace | awk "NR==2{print \"MB=\" \$3}"; df -i /workspace | awk "NR==2{print \"INODES=\" \$2 \" USED=\" \$3}"' 2>&1)
    nfiles=$(printf '%s' "$got" | sed -n 's/^FILES=//p'); mb=$(printf '%s' "$got" | sed -n 's/^MB=//p'); inodes=$(printf '%s' "$got" | sed -n 's/^INODES=//p')
    if [ -z "$nfiles" ]; then
        # An exec that produced nothing is a failed probe, not a
        # workspace that admitted zero files (the first run recorded
        # "admitted  files" and moved on).
        bad "the fill probe produced no FILES= line; the exec said: $(printf '%s' "$got" | tr '\n' ' ' | cut -c1-260)"
    elif [ "$nfiles" -gt 0 ] && [ "$nfiles" -lt 90000 ]; then
        ok "the 1 GiB workspace admitted $nfiles files and then ENOSPC, with ${mb} MiB of 1024 used — the ceiling that arrives first is INODES ($inodes), not bytes"
    else
        bad "the workspace admitted $nfiles files (${mb} MiB used, inodes $inodes): no ceiling was reached in 90000 files, so this leg measured nothing"
    fi
    du1=$(onnode "df -m / | awk 'NR==2{print \$3}'")
    # Containment, not zero: 65k inodes and their directory entries are
    # real metadata blocks. What must NOT happen is the node paying the
    # workspace's APPARENT gigabyte for a tree holding 2 MiB of data.
    [ "$(( du1 - du0 ))" -lt 512 ] \
        && ok "the node's root filesystem moved by $(( du1 - du0 )) MiB while the workspace filled to its ceiling: the image is sparse, the node pays for metadata actually written and not for the 1024 MiB the workspace advertises, and the ENOSPC stayed inside the tenant" \
        || bad "the node's root filesystem grew by $(( du1 - du0 )) MiB filling a 1 GiB workspace — the ceiling is not contained"
else
    bad "tiny-agent never reached Running: $(mount_events tiny-agent | tail -1 | cut -c1-200)"
fi
poddel tiny-agent

# ── L7 what preservation costs, and who pays it ───────────────────────
leg L7 "an undrained tree is preserved forever: a second preservation does not reclaim the first, nothing else does either, and the node's own disk is the budget"
poddel keep-agent keep-agent2
# BOTH prefixes this leg seeds. A prefix left seeded by an earlier run
# is checked out by the new pod, which then reports SEED PRESENT and
# never publishes — the leg waits for a publish that will not come.
mcx mc rm --recursive --force "m/$BUCKET/tenants/keep/" >/dev/null 2>&1
mcx mc rm --recursive --force "m/$BUCKET/tenants/keep2/" >/dev/null 2>&1
und() { local v; v=$(onnode "du -xsk /var/lib/kubelet/plugins/s3.csi.chert.us/undrained 2>/dev/null | awk '{print \$1}'"); printf '%s' "${v:-0}"; }
undn() { onnode "ls /var/lib/kubelet/plugins/s3.csi.chert.us/undrained 2>/dev/null | wc -l"; }
undls() { onnode "ls /var/lib/kubelet/plugins/s3.csi.chert.us/undrained 2>/dev/null"; }
k0=$(und); n0=$(undn); before=$(undls)
[ -n "$k0" ] && ok "PRECONDITION: the node's undrained store holds ${n0} tree(s), $(( ${k0:-0} / 1024 )) MiB" || bad "PRECONDITION: cannot read the undrained store"
leanpod keep-agent proj-keep "$NODE" 50 keep-1
if wait_phase keep-agent Running 400 && wait_seedlog keep-agent PUBLISHED 300; then
    tsh keep-agent "echo unattested > /workspace/src/unattested.txt"
    # No drain can attest: the syncer is killed outright, which is the
    # eviction/OOM shape S20 covers on kind. What is NOT covered
    # anywhere is what the preserved tree then costs.
    # FREEZE, do not kill. A killed syncer is relaunched by its worker
    # and drains normally at the delete — the product working, and the
    # reason this leg saw no preservation when it used a kill. The
    # unattested shape needs a syncer that is alive and cannot act, so
    # the drain runs out its budget and the plugin preserves.
    w=$(worker_of keep-agent); sig_syncer "$w" STOP; sleep 5
    poddel keep-agent
    # The plugin waits the drain budget out (grace + 30 s) before it
    # preserves, so this wait must outlast it.
    i=0; while [ $i -lt 600 ] && [ "$(undn)" = "$n0" ]; do sleep 5; i=$((i + 5)); done
    onnode_ready "$NODE" 120 || bad "the node shell stopped answering mid-leg — every count below would be an empty string, not a zero"
    n1=$(undn); k1=$(und)
    [ "${n1:-0}" -gt "${n0:-0}" ] \
        && ok "the unattested tree was PRESERVED rather than deleted (${n0} → ${n1} trees, $(( ${k0:-0} / 1024 )) → $(( ${k1:-0} / 1024 )) MiB): the tenant's last writes are still on the node, which is the point" \
        || bad "no tree was preserved (${n0} → ${n1}) — the unattested path did not run and this leg measures nothing"
    # THIS leg's tree, by set difference: a bare count credits any
    # preservation, and L6's full workspace produces one of its own (a
    # drain cannot write its attestation to a filesystem that is out of
    # space). The first run counted that tree and would have passed on
    # it, had the content check below not been there.
    mine=$(comm -13 <(printf '%s\n' "$before" | sort) <(undls | sort) | head -1)
    [ -n "$mine" ] && ok "the preserved tree this leg produced is $mine" || bad "no NEW undrained directory appeared for this leg (before: $(printf '%s' "$before" | tr '\n' ' '))"
    # What is preserved is an UNMOUNTED ext4 image, not a directory: an
    # operator cannot `ls` it, and the recovery is a read-only loop
    # mount. Assert that recovery, because a preserved tree nobody can
    # open is not a preserved tree. One simple command per call: a
    # compound command through the node shell came back empty, and an
    # empty answer read as "the image will not mount".
    onnode "mkdir -p /tmp/undr-check" >/dev/null 2>&1
    onnode "mount -o loop,ro /var/lib/kubelet/plugins/s3.csi.chert.us/undrained/$mine/tree.img /tmp/undr-check" >/dev/null 2>&1
    rec=$(onnode "cat /tmp/undr-check/src/unattested.txt")
    [ "$rec" = "unattested" ] \
        && ok "the preserved image loop-mounts read-only and still holds the unpublished write — the recovery an operator would perform, and the reason preserving beats deleting" \
        || bad "the preserved image did not give back the unpublished write (read '$rec')"
    onnode "umount /tmp/undr-check" >/dev/null 2>&1
    # A DIFFERENT workspace: the first one's lease is still held by the
    # syncer this leg killed, so a fresh pod on it would sit out the
    # quiet polls before it could even start — and this leg is about
    # what happens to the PRESERVED tree, not about takeover timing.
    poddel keep-agent2
    leanpod keep-agent2 proj-keep2 "$NODE" 50 keep-2
    if wait_phase keep-agent2 Running 400; then
        w=$(worker_of keep-agent2); [ -n "$w" ] && sig_syncer "$w" STOP && sleep 5
    else
        bad "keep-agent2 never reached Running: $(mount_events keep-agent2 | tail -1 | cut -c1-200)"
    fi
    poddel keep-agent2
    i=0; while [ $i -lt 600 ] && [ "$(undn)" = "$n1" ]; do sleep 5; i=$((i + 5)); done
    onnode_ready "$NODE" 120 || bad "the node shell stopped answering mid-leg"
    n2=$(undn); k2=$(und)
    [ "${n2:-0}" -gt "${n1:-0}" ] \
        && ok "a SECOND preservation does not reclaim the first: ${n2} trees, $(( ${k2:-0} / 1024 )) MiB, and the driver has no expiry, no cap and no reclaim verb for any of them (state.rs: \"an UNDRAINED tree is never removed\")" \
        || bad "the second preservation did not land (${n1} → ${n2})"
    avail=$(onnode "df -Pk / | awk 'NR==2{print \$4}'"); pct=$(onnode "df -P / | awk 'NR==2{print \$5}'")
    per=$(( (${k2:-0} - ${k0:-0}) / 2 )); [ "$per" -lt 1 ] && per=1
    note "the node's root filesystem is $(( ${avail:-0} / 1024 )) MiB free at $pct; these trees average $(( per / 1024 )) MiB each, so about $(( ${avail:-0} / per )) more preservations fit before kubelet's DiskPressure threshold — and DiskPressure EVICTS TENANTS, including tenants that have nothing to do with the workspace that was preserved (measured on this node: reader and tiny-agent were both evicted this way)"
    note "OPERATOR SURFACE: none. Nothing lists these trees, nothing ages them out, and no event fires as they accumulate — only the DrainNotAttested event at the moment each one is written. The disk is the budget and the operator is the garbage collector"
else
    bad "keep-agent never seeded: $(seedlog keep-agent '' | tail -1) $(mount_events keep-agent | tail -1 | cut -c1-200)"
fi

# ── L2 lean, HARD node loss (last: it destroys NODE2) ────────────────
leg L2 "lean under a HARD node loss: $NODE2 is powered off from under kubelet (no shutdown, no drain); the unpublished write is LOST, the lease is left unreleased, and the replacement on $NODE waits it out, ROTATES, and checks out the published set intact"
# NOT `terminate-instances`. An EC2 terminate is a GRACEFUL shutdown:
# the guest gets an ACPI power button, kubelet's graceful node shutdown
# runs, the worker's preStop drains, and the unpublished write is SAVED.
# Measured here on 2026-09-04 — the drain's generation landed in the
# bucket ONE SECOND after the terminate call, marked boundary_source=
# drain, and the leg that assumed a loss failed on its own premise. That
# path is L1's. The path nothing had ever tested is the machine simply
# stopping: a sysrq power-off is serviced in the kernel and gives
# userspace, kubelet and the syncer nothing at all.
$K -n $NS delete deploy lean-loss --ignore-not-found --wait=true --timeout=300s >/dev/null 2>&1
mcx mc rm --recursive --force "m/$BUCKET/tenants/loss/" >/dev/null 2>&1
obs=$($K -n $SYS get pod mc-s3 -o jsonpath='{.spec.nodeName}' 2>/dev/null)
[ -n "$obs" ] && [ "$obs" != "$NODE2" ] \
    && ok "PRECONDITION: the bucket observer runs on $obs, not on the node this leg destroys — a window that dies with what it watches answers 'nothing', and nothing reads as 'the object is not there'" \
    || bad "PRECONDITION: the bucket observer is on '$obs' and this leg kills $NODE2: every bucket assertion below would be blind"
onnode_ready "$NODE2" 300 && ok "PRECONDITION: a node shell answers on $NODE2 (the kill goes through it)" || bad "PRECONDITION: no node shell on $NODE2"
sed -e "s#__NODE__#$NODE2#g" -e "s#__FILES__#200#g" -e "s#__NONCE__#loss-1#g" hd-lean-deploy.yaml.tpl | $K apply -f - >/dev/null
i=0; lp=""; while [ $i -lt 400 ]; do lp=$($K -n $NS get pods -l app=lean-loss -o json 2>/dev/null | jq -r '.items[] | select(.status.phase=="Running") | .metadata.name' | head -1); [ -n "$lp" ] && break; sleep 5; i=$((i + 5)); done
if [ -n "$lp" ] && [ "$($K -n $NS get pod "$lp" -o jsonpath='{.spec.nodeName}')" = "$NODE2" ] && wait_seedlog "$lp" PUBLISHED 400; then
    ok "PRECONDITION: $lp is Running on $NODE2 and its seed is published"
    h0=$(lepoch tenants/loss .holder_id); e0=$(lepoch tenants/loss .epoch); s0=$(lmseq tenants/loss); n0=$(lments tenants/loss)
    [ -n "$h0" ] && [ "${n0:-0}" -ge 201 ] && lmhas tenants/loss src/seeded && ok "PRECONDITION: holder $h0 at epoch $e0, seq $s0, $n0 entries" || bad "PRECONDITION: holder '$h0', entries '$n0'"
    iid=$(instance_of "$NODE2"); [ -n "$iid" ] && [ "$iid" != "None" ] && ok "PRECONDITION: $NODE2 is instance $iid" || bad "PRECONDITION: no instance id for $NODE2"
    tsh "$lp" "echo late > /workspace/src/late.txt"
    sleep 15
    [ -z "$(lobj tenants/loss/files/src/late.txt)" ] && ok "late.txt is in the tree and NOT in the bucket 15s later (floorSecs is an hour: nothing publishes it but a drain or a declared publish)" || bad "late.txt reached the bucket before the kill"
    # The kill. sysrq 'o' powers the machine off from inside the kernel:
    # no SIGTERM, no preStop, no drain, no lease release.
    onnode_at "$NODE2" "sysctl -w kernel.sysrq=1 >/dev/null 2>&1; nohup sh -c 'sleep 2; echo o > /proc/sysrq-trigger' >/dev/null 2>&1 &" >/dev/null 2>&1
    t0=$(now); note "powered off $NODE2 ($iid) at $(date -u +%T) with sysrq"
    i=0; while [ $i -lt 300 ] && node_ready "$NODE2"; do sleep 5; i=$((i + 5)); done
    node_ready "$NODE2" && bad "PRECONDITION: $NODE2 is still Ready ${i}s after the power-off — the kill did not land, and everything below would be about a live node" || ok "PRECONDITION: $NODE2 went NotReady ${i}s after the power-off"
    sleep 30
    [ "$(lmseq tenants/loss)" = "$s0" ] && [ -z "$(lobj tenants/loss/files/src/late.txt)" ] \
        && ok "NO drain ran: the manifest is still at seq $s0 and late.txt is still absent from the bucket $(( $(now) - t0 ))s after the power-off — this is the loss shape, which a graceful terminate never produces" \
        || bad "something published after the power-off (seq $s0 → $(lmseq tenants/loss), late.txt '$(lobj tenants/loss/files/src/late.txt)') — the kill was not hard"
    [ "$(lepoch tenants/loss .released)" = "false" ] \
        && ok "the lease is UNRELEASED: the successor faces a possibly-live straggler, which is exactly what rotation is for" \
        || bad "the lease reads released=$(lepoch tenants/loss .released) — a clean handoff, not a loss"
    i=0; p2=""; while [ $i -lt 1500 ]; do p2=$($K -n $NS get pods -l app=lean-loss -o json 2>/dev/null | jq -r '.items[] | select(.status.phase=="Running") | select(.spec.nodeName=="'"$NODE"'") | .metadata.name' | head -1); [ -n "$p2" ] && break; sleep 10; i=$((i + 10)); done
    if [ -n "$p2" ]; then
        ok "the replacement $p2 is Running on $NODE $(( $(now) - t0 ))s after the power-off (eviction, then the successor waits the dead lease out, then a checkout)"
        [ "$(seedlog "$p2" PRESENT)" != "" ] && ok "it found the published seed by CHECKOUT on a machine that never held this workspace" || bad "the replacement did not find the seed: $(seedlog "$p2" '' | tail -1)"
        [ "$(inpod "$p2" cat /workspace/src/d0/f42.txt)" = "unit 000042 of the seeded project" ] && ok "f42 carries the seeded bytes on the new node" || bad "f42 reads '$(inpod "$p2" cat /workspace/src/d0/f42.txt)'"
        fi=$(files_in "$p2"); [ "$fi" = "201" ] && ok "the published set is intact: 201 files" || bad "the replacement's tree has $fi files, not 201"
        [ "$(inpod "$p2" 'test -f /workspace/src/late.txt && echo present || echo absent')" = "absent" ] \
            && ok "late.txt is NOT there, and that is the honest cost of a hard loss: everything written since the last publish dies with the machine. A graceful shutdown drains it (L1); a power cut cannot" \
            || bad "late.txt appeared on the replacement, so something published it after all"
        h1=$(lepoch tenants/loss .holder_id); e1=$(lepoch tenants/loss .epoch); s1=$(lmseq tenants/loss)
        [ -n "$h1" ] && [ "$h1" != "$h0" ] && ok "a NEW holder ($h1) — the dead machine's lease was superseded" || bad "the holder did not change ($h0 → '$h1')"
        [ "${e1:-0}" -gt "${e0:-0}" ] && [ "$s1" != "$s0" ] && ok "epoch $e0 → $e1 and the manifest ROTATED (seq $s0 → $s1): an unreleased lease is a possibly-live straggler, so the successor rotates before it publishes" || bad "epoch/seq did not move as a takeover should ($e0/$s0 → $e1/$s1)"
        [ "$(lments tenants/loss)" = "$n0" ] && ! lmhas tenants/loss src/late.txt && ok "the rotated manifest still cites $n0 entries and does not cite late.txt" || bad "entries after the takeover: $(lments tenants/loss) (was $n0); late.txt cited: $(lmhas tenants/loss src/late.txt && echo yes || echo no)"
        tsh "$p2" "echo after-loss > /workspace/src/after.txt"; t1=$(now)
        declare_publish "$p2" loss-2 300 && [ "$(lobj tenants/loss/files/src/after.txt)" = "after-loss" ] && ok "the successor publishes ($(( $(now) - t1 ))s to ack; after.txt is in the bucket)" || bad "the successor's publish did not land"
    else
        bad "no replacement Running on $NODE ${i}s after the power-off: $($K -n $NS get pods -l app=lean-loss --no-headers 2>/dev/null | tr '\n' ';')"
    fi
    # The machine is off but the instance still exists, and its Node
    # object outlives it: without a cloud-controller-manager nothing
    # deletes either. Both are the operator's to clean up, and a Node
    # object left behind stalls the next DaemonSet roll.
    $AWSR ec2 terminate-instances --instance-ids "$iid" >/dev/null 2>&1 && note "terminated the powered-off instance $iid"
    note "the dead node's Node object stays (no cloud-controller-manager): deleting it so the cluster's DaemonSets can roll"
    $K delete node "$NODE2" --ignore-not-found >/dev/null 2>&1
else
    bad "PRECONDITION: no Running lean-loss pod on $NODE2 with a published seed: pod '$lp' on '$($K -n $NS get pod "$lp" -o jsonpath='{.spec.nodeName}' 2>/dev/null)', $(seedlog "$lp" '' | tail -1)"
fi
$K -n $NS delete deploy lean-loss --ignore-not-found --wait=false >/dev/null 2>&1

# ── roster ────────────────────────────────────────────────────────────
echo
for want in L1 L2 L3 L4 L5 L6 L7; do echo " $RAN_LEGS " | grep -q " $want " || bad "leg $want never ran"; done
echo "════════════════════════════════════════"
echo "flint hardening on real nodes: $PASS ok, $FAILED bad, $SKIPPED skipped"
[ "$FAILED" = "0" ]
