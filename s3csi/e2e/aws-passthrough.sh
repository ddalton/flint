#!/usr/bin/env bash
# flint-passthrough on REAL nodes — every aspect the kind rig cannot reach.
#
# Runs AFTER `run-s3csi.sh setup` (STORE=s3 NODE_EXEC=nodesh) on an EC2
# cluster with TWO workers, against three real buckets: the main one, an
# SSE-KMS one, a cross-region one. The helpers are the single-cluster
# drill's own, evaluated from run-s3csi.sh at run time rather than
# copied — run-multi.sh's private copies are how a resolver drifted.
#
#   P1  throughput: 512 MiB through the mount, byte-identical, above a floor
#   P2  many files: a 5000-object prefix lists complete and reads correct
#   P3  16 tenants on one node, each its own worker, all reclaimed
#   P4  a tenant container restart keeps its mount
#   P5  a kubelet restart mid-read leaves mounts serving; the driver re-registers
#   P6  a REAL node reboot: the tenant comes back mounted with a fresh worker
#   P7  instance TERMINATION — a GRACEFUL shutdown, not a hard loss: the
#       instance is terminated and a Deployment tenant reschedules
#   P8  rotation soak: 30 min of reads across ≥10 key rotations, zero errors
#   P9  ambient identity: the worker is handed nothing; the platform's own
#       credential chain admits it (precondition: the platform can complete it)
#   P10 SSE-KMS bucket: writes land encrypted with the bucket's key, reads work
#   P11 cross-region bucket: mounts and serves from another region
#   P12 a VPC gateway endpoint for S3 in the cluster's VPC: mounts keep serving
#   P13 an S3 partition on the node: reads fail while it holds, resume after
#   P14 broker HA: two replicas rolled mid-read, rotation continues
#
# Env: KUBECONFIG CTX BUCKET S3_REGION S3_KEY_FILE (as run-s3csi.sh, STORE=s3
# NODE_EXEC=nodesh forced), plus BK (the KMS bucket), B2 (the us-east-1
# bucket), KMSKEY, NODE2 (the second worker), TROVE_ADMIN (an AWS profile
# that may create a VPC endpoint and read object encryption; the
# rolesanywhere profile terminates the instance in P7).
set -u
cd "$(dirname "$0")"
export STORE=s3 NODE_EXEC=nodesh
: "${BK:?}" "${B2:?}" "${KMSKEY:?}" "${NODE2:?}" "${TROVE_ADMIN:=trove-admin}"
REPO=$(cd ../.. && pwd)
# The drill's knobs and helpers, verbatim, up to its setup block.
eval "$(sed -n '/^CTX=\${CTX:-/,/^# ── setup \/ teardown/p' run-s3csi.sh | sed '$d')"
eval "$(grep -E '^(lobj|lcount)\(\) ' run-s3csi.sh)"
AWSA="env AWS_PROFILE=$TROVE_ADMIN AWS_REGION=$S3_REGION aws"
AWSR="env AWS_PROFILE=rolesanywhere AWS_REGION=$S3_REGION aws"
AWSK="env -u AWS_PROFILE AWS_ACCESS_KEY_ID=$AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY=$AWS_SECRET_ACCESS_KEY aws"
now() { date +%s; }
SKIPPED=0
skip() { SKIPPED=$((SKIPPED + 1)); echo "  SKIP: $1"; }
ptcrs() { sed -e "s#__B__#$BUCKET#g" -e "s#__BK__#$BK#g" -e "s#__B2__#$B2#g" -e "s#__REGION__#$S3_REGION#g" -e "s#__KMSKEY__#$KMSKEY#g" pt-tenants.yaml.tpl; }
ptpod() { sed -e "s#__NAME__#$1#g" -e "s#__CR__#$2#g" -e "s#__NODE__#$3#g" pt-pod.yaml.tpl | $K apply -f - >/dev/null; }
ptdel() { $K -n $NS delete pod "$@" --ignore-not-found --wait=true --timeout=240s >/dev/null 2>&1; }
node_ready() { [ "$($K get node "$1" -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}' 2>/dev/null)" = "True" ]; }
restarts() { $K -n $NS get pod "$1" -o jsonpath='{.status.containerStatuses[0].restartCount}' 2>/dev/null; }
mounts_on_node() { onnode "grep -c 'plugins/s3.csi.chert.us/volumes' /proc/mounts"; }
echo "flint-passthrough on real nodes — cluster $CTX, node $NODE (+$NODE2), buckets $BUCKET / $BK / $B2"
$K get csidriver s3.csi.chert.us >/dev/null 2>&1 || { echo "no s3.csi.chert.us — run run-s3csi.sh setup first"; exit 2; }
ptcrs | $K apply -f - >/dev/null

# ── P8 (started first, collected before P7): the soak reader on NODE2 ─
leg P8 "rotation soak: a reader on $NODE2 reads every 5 s for 30 min across ≥10 key rotations (lifetime ${CREDS_LIFETIME}s) with zero errors"
ptpod pt-soak datasets "$NODE2"
if wait_phase pt-soak Running 300; then
    soak_issued0=$(broker_issued); soak_t0=$(now)
    soak_out=$(mktemp)
    ( $K -n $NS exec pt-soak -c agent -- /bin/sh -c 'e=0; n=0; for i in $(seq 1 360); do n=$((n+1)); [ "$(cat /mnt/s3/shard-01.txt)" = seeded-object-01 ] || e=$((e+1)); sleep 5; done; echo "errors=$e reads=$n"' > "$soak_out" 2>&1 ) &
    soak_pid=$!
    ok "PRECONDITION: the soak reader is Running on $NODE2 and reading (broker issued so far: ${soak_issued0:-?})"
else
    bad "the soak reader never reached Running — P8 makes no observation"; soak_pid=""
fi

# ── P1 throughput ─────────────────────────────────────────────────────
leg P1 "throughput: 512 MiB written through the mount lands whole and reads back byte-identical; a 256 MiB object written by someone else reads whole; both directions clear 20 MiB/s"
ptpod pt-rw pt-rw "$NODE"
if wait_phase pt-rw Running 300; then
    src=$(inpod_out pt-rw "dd if=/dev/urandom of=/tmp/src bs=1M count=512 2>/dev/null; md5sum /tmp/src | cut -c1-32")
    [ -n "$src" ] && ok "PRECONDITION: 512 MiB of random bytes staged in the tenant ($src)" || bad "PRECONDITION: could not stage the source file"
    t0=$(now); tsh pt-rw "cp /tmp/src /mnt/s3/big"; rc=$?; t1=$(now)
    wsec=$((t1 - t0)); [ $wsec -lt 1 ] && wsec=1
    size=$(mcx mc stat --json "m/$BUCKET/pt/rw/big" 2>/dev/null | jq -r '.size // 0')
    [ $rc -eq 0 ] && [ "$size" = "536870912" ] && ok "the write landed as one 536870912-byte object in ${wsec}s ($((512 / wsec)) MiB/s)" || bad "write rc=$rc, object size ${size:-?} (expected 536870912) after ${wsec}s"
    [ $((512 / wsec)) -ge 20 ] && ok "write throughput $((512 / wsec)) MiB/s clears the 20 MiB/s floor" || bad "write throughput $((512 / wsec)) MiB/s is below the 20 MiB/s floor"
    t0=$(now); back=$(inpod_out pt-rw "md5sum /mnt/s3/big | cut -c1-32"); t1=$(now); rsec=$((t1 - t0)); [ $rsec -lt 1 ] && rsec=1
    [ -n "$back" ] && [ "$back" = "$src" ] && ok "read back byte-identical ($back) in ${rsec}s ($((512 / rsec)) MiB/s)" || bad "read back '$back' ≠ written '$src'"
    [ $((512 / rsec)) -ge 20 ] && ok "read throughput $((512 / rsec)) MiB/s clears the 20 MiB/s floor" || bad "read throughput $((512 / rsec)) MiB/s is below the 20 MiB/s floor"
    seed=$(mcx sh -c "dd if=/dev/urandom bs=1M count=256 2>/dev/null | tee /tmp/seed | mc pipe m/$BUCKET/pt/rw/seeded >/dev/null; md5sum /tmp/seed | cut -c1-32; rm -f /tmp/seed")
    got=$(inpod_out pt-rw "md5sum /mnt/s3/seeded | cut -c1-32")
    [ -n "$seed" ] && [ "$got" = "$seed" ] && ok "a 256 MiB object written by ANOTHER writer reads whole and identical ($got)" || bad "foreign 256 MiB object: read '$got' ≠ written '$seed'"
    tsh pt-rw "rm /mnt/s3/big /mnt/s3/seeded; rm -f /tmp/src"
    [ "$(lcount pt/rw/)" = "0" ] && ok "unlink through the mount removed both objects from the bucket" || bad "$(lcount pt/rw/) object(s) remain under pt/rw/ after unlink"
else
    bad "pt-rw never reached Running: $(mount_events pt-rw | tail -1 | cut -c1-160)"
fi

# ── P2 many files ─────────────────────────────────────────────────────
leg P2 "many files: a 5000-object prefix lists complete through the mount and a sample reads correct"
mcx sh -c 'rm -rf /tmp/many; mkdir -p /tmp/many && cd /tmp/many && i=1; while [ $i -le 5000 ]; do echo "obj-$i" > f$i; i=$((i+1)); done; mc cp -q -r /tmp/many/ m/'"$BUCKET"'/pt/many/ >/dev/null 2>&1; rm -rf /tmp/many' >/dev/null 2>&1
n0=$(lcount pt/many/)
[ "$n0" = "5000" ] && ok "PRECONDITION: 5000 objects seeded under pt/many/" || bad "PRECONDITION: $n0 objects under pt/many/, not 5000"
ptpod pt-many pt-many "$NODE"
if wait_phase pt-many Running 300; then
    t0=$(now); n=$(inpod_out pt-many "ls /mnt/s3 | wc -l" | tr -d ' '); t1=$(now)
    [ "$n" = "5000" ] && ok "the mount lists all 5000 entries ($((t1 - t0))s)" || bad "the mount lists $n entries, not 5000"
    e=$(inpod_out pt-many 'e=0; for i in 1 7 77 777 2500 4999 5000; do [ "$(cat /mnt/s3/f$i)" = "obj-$i" ] || e=$((e+1)); done; echo $e')
    [ "$e" = "0" ] && ok "seven sampled files read their own bytes" || bad "$e of 7 sampled files read wrong bytes"
    ptdel pt-many
else
    bad "pt-many never reached Running: $(mount_events pt-many | tail -1 | cut -c1-160)"
fi

# ── P3 many tenants ───────────────────────────────────────────────────
leg P3 "16 tenants on one node: every mount serves its bytes, each tenant has its own worker, and all of it is reclaimed at delete"
m0=$(mounts_on_node); free0=$(onnode "free -m | awk '/Mem:/{print \$7}'")
for i in $(seq 1 16); do ptpod "pt-t$i" datasets "$NODE"; done
i=0; while [ $i -lt 400 ] && [ "$($K -n $NS get pods -l suite=pt --no-headers 2>/dev/null | grep -c "^pt-t.* Running")" -lt 16 ]; do sleep 5; i=$((i + 5)); done
running=$($K -n $NS get pods --no-headers 2>/dev/null | grep -c "^pt-t.* Running")
[ "$running" = "16" ] && ok "all 16 tenants Running on $NODE within ${i}s" || bad "$running of 16 tenants Running after ${i}s: $($K -n $NS get pods --no-headers | grep '^pt-t' | grep -v Running | head -3 | tr '\n' ';')"
e=0; ws=""
for i in $(seq 1 16); do [ "$(inpod_out "pt-t$i" cat /mnt/s3/shard-05.txt)" = "seeded-object-05" ] || e=$((e + 1)); ws="$ws $(worker_of "pt-t$i")"; done
[ $e -eq 0 ] && ok "all 16 read shard-05.txt with its bytes" || bad "$e of 16 tenants read wrong or no bytes"
nw=$(echo "$ws" | tr ' ' '\n' | grep -c .); nu=$(echo "$ws" | tr ' ' '\n' | grep . | sort -u | wc -l | tr -d ' ')
[ "$nw" = "16" ] && [ "$nu" = "16" ] && ok "16 workers, all distinct — one per tenant" || bad "$nw worker(s) for 16 tenants, $nu distinct"
free1=$(onnode "free -m | awk '/Mem:/{print \$7}'"); note "node available memory: ${free0} MiB → ${free1} MiB with 16 mounts"
ptdel $(seq -f 'pt-t%g' 1 16)
i=0; while [ $i -lt 120 ] && [ "$($K -n $WNS get pods -o json 2>/dev/null | jq '[.items[] | select(.metadata.annotations["chert.us/tenant-pod"] | test("s3-tenants/pt-t"))] | length')" != "0" ]; do sleep 5; i=$((i + 5)); done
left=$($K -n $WNS get pods -o json 2>/dev/null | jq '[.items[] | select(.metadata.annotations["chert.us/tenant-pod"] | test("s3-tenants/pt-t"))] | length')
[ "$left" = "0" ] && ok "all 16 workers gone ${i}s after the tenants" || bad "$left worker(s) remain for deleted pt-t tenants"
m2=$(mounts_on_node)
[ "$m2" = "$m0" ] && ok "plugin-dir mounts on the node back to $m0" || bad "plugin-dir mounts $m0 → $m2 after 16 tenants came and went"

# ── P4 tenant container restart ───────────────────────────────────────
leg P4 "a tenant container restart keeps its mount: same pod, same worker, bytes still there"
if require_pod reader; then
    w0=$(worker_of reader); rc0=$(restarts reader)
    $K -n $NS exec reader -c agent -- /bin/sh -c 'kill -TERM 1' >/dev/null 2>&1
    i=0; while [ $i -lt 90 ] && [ "$(restarts reader)" = "$rc0" ]; do sleep 3; i=$((i + 3)); done
    [ "$(restarts reader)" != "$rc0" ] && ok "PRECONDITION: the agent container restarted ($rc0 → $(restarts reader))" || bad "PRECONDITION: the container did not restart in 90s"
    wait_phase reader Running 120 >/dev/null
    [ "$(inpod_out reader cat /mnt/s3/shard-05.txt)" = "seeded-object-05" ] && ok "the restarted container reads its mount" || bad "the restarted container cannot read its mount"
    [ "$(worker_of reader)" = "$w0" ] && ok "same worker ($w0): a container restart is not a republish" || bad "worker changed across the container restart ($w0 → $(worker_of reader))"
fi

# ── P5 kubelet restart ────────────────────────────────────────────────
leg P5 "a kubelet restart on $NODE mid-read leaves every mount serving, and the driver re-registers for the next tenant"
if require_pod reader; then
    s0=$(inpod_out reader "cat /mnt/s3/shard-0*.txt | md5sum | cut -c1-32")
    onnode "systemctl restart kubelet" >/dev/null 2>&1; sleep 4
    s1=$(inpod_out reader "cat /mnt/s3/shard-0*.txt | md5sum | cut -c1-32")
    i=0; while [ $i -lt 120 ] && ! node_ready "$NODE"; do sleep 3; i=$((i + 3)); done
    node_ready "$NODE" && ok "kubelet is back and $NODE is Ready (${i}s)" || bad "$NODE not Ready 120s after the kubelet restart"
    s2=$(inpod_out reader "cat /mnt/s3/shard-0*.txt | md5sum | cut -c1-32")
    [ -n "$s0" ] && [ "$s0" = "$s1" ] && [ "$s1" = "$s2" ] && ok "reads before/during/after the kubelet restart agree ($s0)" || bad "reads diverged across the kubelet restart: $s0 / $s1 / $s2"
    ptpod pt-after datasets "$NODE"
    wait_phase pt-after Running 180 && [ "$(inpod_out pt-after cat /mnt/s3/shard-05.txt)" = "seeded-object-05" ] && ok "a NEW tenant mounts and reads after the restart (the driver re-registered)" || bad "a new tenant could not mount after the kubelet restart: $(mount_events pt-after | tail -1 | cut -c1-160)"
    ptdel pt-after
fi

# ── P9 ambient identity ───────────────────────────────────────────────
leg P9 "ambient identity: the worker is handed nothing — no broker, no key on the pod — and the platform's own credential chain admits reads and writes"
# PRECONDITION, and a generic one. Nothing in flint names a cloud: an
# ambient worker runs the SDK's default chain (environment, shared file,
# web-identity token, container credential URI, instance metadata, in
# that order) and whatever supplies credentials to a pod on this node is
# the platform's business — an on-premise rack satisfies it with a file
# or the environment, EC2 without IRSA falls through to instance
# metadata. So the probe is a pod on NODE with nothing injected asking
# that same chain for an identity. If the platform cannot complete the
# exchange, the leg records it and stops: run on, it would judge the
# platform, not flint. (The first EC2 run did exactly that: trove's
# IMDSv2 hop limit of 2 let the metadata GETs reach the pod through
# Cilium's tunnel but not the token PUT's response, so the chain ended
# with nothing and P9 read as a flint failure.)
$K -n $NS delete pod pt-ambient-probe --ignore-not-found --wait=true --timeout=60s >/dev/null 2>&1
# Restricted-admissible like every tenant pod (the namespace enforces
# the profile); a probe the namespace refuses is a drill defect, not a
# platform verdict, and is reported as one.
probe_err=$(cat <<EOF | $K apply -f - 2>&1 >/dev/null
apiVersion: v1
kind: Pod
metadata: { name: pt-ambient-probe, namespace: $NS }
spec:
  nodeName: $NODE
  restartPolicy: Never
  securityContext: { runAsNonRoot: true, runAsUser: 1001, seccompProfile: { type: RuntimeDefault } }
  containers:
  - name: probe
    image: amazon/aws-cli:2.36.39
    command: ["sh", "-c", "aws sts get-caller-identity --output text 2>&1 || true"]
    env: [{ name: AWS_REGION, value: $S3_REGION }, { name: HOME, value: /tmp }]
    securityContext: { allowPrivilegeEscalation: false, capabilities: { drop: [ALL] } }
EOF
)
probe=__never__
if [ -n "$probe_err" ]; then
    bad "PRECONDITION: the ambient probe could not be created: $(echo "$probe_err" | cut -c1-200)"
else
    i=0; ph=""; while [ $i -lt 300 ]; do ph=$($K -n $NS get pod pt-ambient-probe -o jsonpath='{.status.phase}' 2>/dev/null); case "$ph" in Succeeded|Failed) break ;; esac; sleep 5; i=$((i + 5)); done
    case "$ph" in
        Succeeded|Failed) probe=$($K -n $NS logs pt-ambient-probe 2>/dev/null | tail -1) ;;
        *) bad "PRECONDITION: the ambient probe never completed in ${i}s (phase '${ph:-none}'): $(mount_events pt-ambient-probe | tail -1 | cut -c1-200)" ;;
    esac
fi
$K -n $NS delete pod pt-ambient-probe --ignore-not-found --wait=false >/dev/null 2>&1
if [ "$probe" = __never__ ]; then
    :
elif echo "$probe" | grep -q 'arn:aws:'; then
    ok "PRECONDITION: a pod on $NODE with nothing injected obtains an identity from the platform's chain ($(echo "$probe" | awk '{print $2}'))"
    # The pipe runs INSIDE the mc pod: `mcx` execs without -i, so a
    # host-side pipe seeds a zero-byte object (the first P9 pass read
    # exactly that back and blamed the mount).
    mcx sh -c "printf 'ambient-seeded\\n' | mc pipe m/$BUCKET/pt/ambient/hello.txt" >/dev/null 2>&1
    ptpod pt-ambient pt-ambient "$NODE"
    if wait_phase pt-ambient Running 300; then
        [ "$(inpod_out pt-ambient cat /mnt/s3/hello.txt)" = "ambient-seeded" ] && ok "the ambient mount reads the seeded object (the platform's identity was admitted to the bucket)" || bad "ambient mount cannot read: '$(inpod_out pt-ambient cat /mnt/s3/hello.txt)'"
        w=$(worker_of pt-ambient)
        creds=$($K -n $WNS exec "$w" -- sh -c 'ls /comm 2>/dev/null | grep -c creds.json' 2>/dev/null)
        [ "${creds:-1}" = "0" ] && ok "no creds.json in the worker's comm dir — nothing was brokered" || bad "the ambient worker has a creds.json: the broker was involved"
        tsh pt-ambient "echo ambient-wrote > /mnt/s3/w.txt"
        [ "$(lobj pt/ambient/w.txt)" = "ambient-wrote" ] && ok "an ambient write landed in the bucket" || bad "the ambient write did not land: '$(lobj pt/ambient/w.txt)'"
        ptdel pt-ambient
    else
        # The event must carry the chain's OWN last word (the plugin
        # waits for the mounter's mount.error rather than racing it).
        bad "pt-ambient never reached Running: $(mount_events pt-ambient | tail -3 | tr '\n' ' ' | cut -c1-400)"
    fi
else
    skip "the platform does not complete the ambient chain for a pod on $NODE; P9 would judge the platform, not flint. The probe said: '${probe:-no output}'"
fi

# ── P10 SSE-KMS ───────────────────────────────────────────────────────
leg P10 "an SSE-KMS bucket: a write through the mount lands encrypted with the bucket's key, and reads back"
ptpod pt-kms pt-kms "$NODE"
if wait_phase pt-kms Running 300; then
    tsh pt-kms "echo kms-bytes > /mnt/s3/enc.txt"
    sse=$($AWSA s3api head-object --bucket "$BK" --key pt/kms/enc.txt --query '[ServerSideEncryption,SSEKMSKeyId]' --output text 2>/dev/null)
    case "$sse" in *aws:kms*"$KMSKEY"*) ok "the object is SSE-KMS under the bucket's key ($KMSKEY)" ;; *) bad "head-object says '$sse' (expected aws:kms with key $KMSKEY)" ;; esac
    [ "$(inpod_out pt-kms cat /mnt/s3/enc.txt)" = "kms-bytes" ] && ok "the encrypted object reads back through the mount" || bad "read back '$(inpod_out pt-kms cat /mnt/s3/enc.txt)'"
    ptdel pt-kms
else
    bad "pt-kms never reached Running: $(mount_events pt-kms | tail -1 | cut -c1-200)"
fi

# ── P11 cross-region ──────────────────────────────────────────────────
leg P11 "a bucket in us-east-1 mounted from $S3_REGION: reads and writes work; per-read latency is recorded"
printf 'far-seeded' | $AWSK s3 cp - "s3://$B2/pt/use1/far.txt" --region us-east-1 >/dev/null 2>&1
ptpod pt-use1 pt-use1 "$NODE"
if wait_phase pt-use1 Running 300; then
    [ "$(inpod_out pt-use1 cat /mnt/s3/far.txt)" = "far-seeded" ] && ok "the cross-region mount reads the seeded object" || bad "cross-region read: '$(inpod_out pt-use1 cat /mnt/s3/far.txt)'"
    t0=$(date +%s%N); inpod_out pt-use1 'for i in $(seq 1 20); do cat /mnt/s3/far.txt >/dev/null; done; echo done' >/dev/null; t1=$(date +%s%N)
    note "20 sequential reads of a 10-byte object across regions: $(( (t1 - t0) / 20000000 )) ms each"
    tsh pt-use1 "echo far-wrote > /mnt/s3/w.txt"
    [ "$($AWSK s3 cp "s3://$B2/pt/use1/w.txt" - --region us-east-1 2>/dev/null)" = "far-wrote" ] && ok "a cross-region write landed in us-east-1" || bad "the cross-region write did not land"
    ptdel pt-use1
else
    bad "pt-use1 never reached Running: $(mount_events pt-use1 | tail -1 | cut -c1-200)"
fi

# ── P12 VPC gateway endpoint ──────────────────────────────────────────
leg P12 "a VPC gateway endpoint for S3 in the cluster's VPC: the route appears and the mounts keep serving through it"
iid=$($AWSR ec2 describe-instances --filters "Name=private-ip-address,Values=$($K get node "$NODE" -o jsonpath='{.status.addresses[?(@.type=="InternalIP")].address}')" --query 'Reservations[0].Instances[0].[VpcId,SubnetId]' --output text 2>/dev/null)
vpc=${iid%%	*}; subnet=${iid##*	}
rtb=$($AWSA ec2 describe-route-tables --filters "Name=association.subnet-id,Values=$subnet" --query 'RouteTables[0].RouteTableId' --output text 2>/dev/null)
[ "$rtb" = "None" ] || [ -z "$rtb" ] && rtb=$($AWSA ec2 describe-route-tables --filters "Name=vpc-id,Values=$vpc" "Name=association.main,Values=true" --query 'RouteTables[0].RouteTableId' --output text 2>/dev/null)
[ -n "$vpc" ] && [ -n "$rtb" ] && ok "PRECONDITION: $NODE is in $vpc, route table $rtb" || bad "PRECONDITION: could not resolve the node's VPC/route table"
vpce=$($AWSA ec2 create-vpc-endpoint --vpc-id "$vpc" --service-name "com.amazonaws.$S3_REGION.s3" --route-table-ids "$rtb" --query VpcEndpoint.VpcEndpointId --output text 2>/dev/null)
i=0; while [ $i -lt 120 ] && [ "$($AWSA ec2 describe-vpc-endpoints --vpc-endpoint-ids "$vpce" --query 'VpcEndpoints[0].State' --output text 2>/dev/null)" != "available" ]; do sleep 5; i=$((i + 5)); done
[ -n "$vpce" ] && ok "gateway endpoint $vpce is available (${i}s)" || bad "no VPC endpoint was created"
pl=$($AWSA ec2 describe-route-tables --route-table-ids "$rtb" --query "RouteTables[0].Routes[?GatewayId=='$vpce'].DestinationPrefixListId" --output text 2>/dev/null)
[ -n "$pl" ] && [ "$pl" != "None" ] && ok "the route table routes the S3 prefix list $pl via the endpoint" || bad "no prefix-list route via $vpce in $rtb"
sleep 10
[ "$(inpod_out reader cat /mnt/s3/shard-05.txt)" = "seeded-object-05" ] && ok "the existing mount keeps serving through the endpoint" || bad "reads broke after the endpoint appeared"
ptpod pt-vpce datasets "$NODE"
wait_phase pt-vpce Running 180 && [ "$(inpod_out pt-vpce cat /mnt/s3/shard-05.txt)" = "seeded-object-05" ] && ok "a NEW mount comes up through the endpoint" || bad "a new mount failed with the endpoint in place: $(mount_events pt-vpce | tail -1 | cut -c1-160)"
ptdel pt-vpce
$AWSA ec2 delete-vpc-endpoints --vpc-endpoint-ids "$vpce" >/dev/null 2>&1 && note "endpoint $vpce deleted"

# ── P13 S3 partition ──────────────────────────────────────────────────
leg P13 "an S3 partition on $NODE: reads fail while the drop rules hold and resume within a minute of their removal"
plid=$($AWSA ec2 describe-managed-prefix-lists --filters "Name=prefix-list-name,Values=com.amazonaws.$S3_REGION.s3" --query 'PrefixLists[0].PrefixListId' --output text 2>/dev/null)
cidrs=$($AWSA ec2 get-managed-prefix-list-entries --prefix-list-id "$plid" --query 'Entries[].Cidr' --output text 2>/dev/null | tr '\t' ' ')
[ -n "$cidrs" ] && ok "PRECONDITION: S3's prefix list $plid has $(echo $cidrs | wc -w | tr -d ' ') CIDRs" || bad "PRECONDITION: could not read S3's prefix list"
# Not iptables OUTPUT: pod traffic under Cilium (vxlan + masquerade) is
# forwarded, never host-originated, and the first run's DROP rules cut
# nothing — a read succeeded under the "partition". A blackhole route is
# consulted for every packet the host forwards, pods' included.
rules=""; for c in $cidrs; do rules="$rules ip route add blackhole $c;"; done
onnode "$rules true" >/dev/null 2>&1
bh=$(onnode "ip route show type blackhole | wc -l"); [ "${bh:-0}" -ge 1 ] && ok "PRECONDITION: $bh blackhole route(s) installed for S3's CIDRs on $NODE" || bad "PRECONDITION: no blackhole route landed — the partition is not in place"
sleep 3
$K -n $NS exec reader -c agent -- timeout 25 cat /mnt/s3/shard-06.txt >/dev/null 2>&1 && bad "a read SUCCEEDED under the partition — the rules did not cut S3 off (or a cache served it)" || ok "a read under the partition fails or stalls (timed out at 25s)"
undo=""; for c in $cidrs; do undo="$undo ip route del blackhole $c;"; done
onnode "$undo true" >/dev/null 2>&1
[ "$(onnode "ip route show type blackhole | wc -l")" = "0" ] && ok "the blackhole routes are gone" || bad "blackhole routes remain on $NODE after the leg"
i=0; while [ $i -lt 90 ] && [ "$($K -n $NS exec reader -c agent -- timeout 10 cat /mnt/s3/shard-06.txt 2>/dev/null)" != "seeded-object-06" ]; do sleep 5; i=$((i + 5)); done
[ "$($K -n $NS exec reader -c agent -- timeout 10 cat /mnt/s3/shard-06.txt 2>/dev/null)" = "seeded-object-06" ] && ok "reads resumed ${i}s after the partition lifted" || bad "reads did not resume within 90s of lifting the partition"
md=$(mount_events reader | grep -c MounterDead || true); note "MounterDead events on reader after the partition: $md"

# ── P14 broker HA ─────────────────────────────────────────────────────
leg P14 "broker HA: two replicas rolled mid-read — the reader keeps reading and key rotation continues"
$K -n $SYS scale deploy/flint-s3-broker --replicas=2 >/dev/null 2>&1; $K -n $SYS rollout status deploy/flint-s3-broker --timeout=180s >/dev/null 2>&1
w=$(worker_of reader); exp0=$($K -n $WNS exec "$w" -- sh -c 'cat /comm/creds.json' 2>/dev/null | python3 -c "import json,sys; print(json.load(sys.stdin)['Expiration'])" 2>/dev/null)
$K -n $SYS rollout restart deploy/flint-s3-broker >/dev/null 2>&1
errs=$(inpod_out reader 'e=0; for i in $(seq 1 12); do [ "$(cat /mnt/s3/shard-01.txt)" = seeded-object-01 ] || e=$((e+1)); sleep 5; done; echo $e')
[ "$errs" = "0" ] && ok "zero read errors across the broker roll" || bad "$errs read errors during the broker roll"
$K -n $SYS rollout status deploy/flint-s3-broker --timeout=180s >/dev/null 2>&1
i=0; exp1=$exp0; while [ $i -lt 240 ] && [ "$exp1" = "$exp0" ]; do sleep 10; i=$((i + 10)); exp1=$($K -n $WNS exec "$w" -- sh -c 'cat /comm/creds.json' 2>/dev/null | python3 -c "import json,sys; print(json.load(sys.stdin)['Expiration'])" 2>/dev/null); done
[ -n "$exp1" ] && [ "$exp1" != "$exp0" ] && ok "rotation continued after the roll (Expiration $exp0 → $exp1, ${i}s)" || bad "no rotation within 240s of the broker roll ($exp0 → $exp1)"
$K -n $SYS scale deploy/flint-s3-broker --replicas=1 >/dev/null 2>&1

# ── P6 node reboot (the node under `reader`) ──────────────────────────
leg P6 "a REAL reboot of $NODE: the tenant on it comes back mounted and reading, with a working worker, and nothing is orphaned"
if require_pod reader; then
    w0=$(worker_of reader); rc0=$(restarts reader)
    onnode "nohup sh -c 'sleep 2; systemctl reboot' >/dev/null 2>&1 &" >/dev/null 2>&1
    i=0; while [ $i -lt 180 ] && node_ready "$NODE"; do sleep 5; i=$((i + 5)); done
    node_ready "$NODE" && bad "PRECONDITION: $NODE never went NotReady — the reboot did not happen" || ok "PRECONDITION: $NODE went NotReady (${i}s) — it is rebooting"
    i=0; while [ $i -lt 420 ] && ! node_ready "$NODE"; do sleep 10; i=$((i + 10)); done
    node_ready "$NODE" && ok "$NODE is Ready again (${i}s)" || bad "$NODE not Ready 420s after the reboot"
    i=0; while [ $i -lt 400 ] && { [ "$(restarts reader)" = "$rc0" ] || [ "$($K -n $NS get pod reader -o jsonpath='{.status.phase}')" != "Running" ]; }; do sleep 10; i=$((i + 10)); done
    [ "$(restarts reader)" != "$rc0" ] && ok "the tenant's container restarted on the rebooted node (${i}s)" || bad "the tenant never restarted after the reboot"
    got=$(inpod_out reader cat /mnt/s3/shard-05.txt)
    [ "$got" = "seeded-object-05" ] && ok "the tenant reads its mount after the reboot" || bad "after the reboot the tenant reads '$got' — the mount did not come back: $(mount_events reader | tail -1 | cut -c1-160)"
    w1=$(worker_of reader)
    [ -n "$w1" ] && ok "a Running worker serves the tenant after the reboot ($w0 → $w1)" || bad "no Running worker for reader after the reboot"
    stale=$(onnode 'cd /var/lib/kubelet/plugins/s3.csi.chert.us/volumes 2>/dev/null && for d in */; do [ -f "$d/state.json" ] || echo "$d"; done | wc -l')
    [ "${stale:-0}" = "0" ] && ok "no half-removed volume directory on the node after the reboot" || bad "$stale volume dir(s) without state after the reboot"
    live=$($K -n $NS get pods -o json | jq '[.items[] | select(.status.phase=="Running") | select(.spec.nodeName=="'"$NODE"'") | select(.spec.volumes[]?.csi.driver=="s3.csi.chert.us")] | length')
    m=$(mounts_on_node); [ "${m:-0}" -le "$live" ] && ok "plugin-dir mounts ($m) do not exceed live CSI tenants on the node ($live)" || bad "$m plugin-dir mounts for $live live tenants after the reboot"
fi

# ── P8 collect ────────────────────────────────────────────────────────
leg P8 "rotation soak — collect"
if [ -n "${soak_pid:-}" ]; then
    wait "$soak_pid" 2>/dev/null; res=$(cat "$soak_out"); rm -f "$soak_out"
    soak_issued1=$(broker_issued); el=$(( $(now) - soak_t0 ))
    case "$res" in errors=0\ reads=360) ok "360 reads over ${el}s with zero errors" ;; *) bad "soak result: '${res:-<none>}' (wanted errors=0 reads=360)" ;; esac
    [ "${soak_issued1:-0}" -ge $(( ${soak_issued0:-0} + 10 )) ] 2>/dev/null && ok "the broker issued ≥10 more keys during the soak (${soak_issued0} → ${soak_issued1}): rotation happened, repeatedly" || bad "broker issued ${soak_issued0} → ${soak_issued1}: fewer than 10 rotations in the soak"
fi
ptdel pt-soak

# ── P7 instance termination (last: it costs the second worker) ────────
# NAMED CAREFULLY. `terminate-instances` is a GRACEFUL shutdown: the
# guest gets an ACPI power button and kubelet's graceful node shutdown
# terminates the pods in priority order before the machine goes. So this
# leg measures a planned instance termination — the spot-reclamation
# shape, which carries a two-minute warning — and NOT a machine that
# simply stops. The hard shape (sysrq power-off, no shutdown, no drain)
# is aws-hardening.sh L2, where it matters far more: a passthrough
# tenant has no unpublished state to lose, and a lean workspace does.
leg P7 "instance TERMINATION (graceful: the guest is shut down, not cut off): the instance under a Deployment-managed tenant on $NODE2 is terminated; the tenant reschedules to $NODE and mounts"
sed -e "s#__NODE__#$NODE2#g" pt-deploy.yaml.tpl | $K apply -f - >/dev/null
i=0; p=""; while [ $i -lt 300 ] && [ -z "$p" ]; do p=$($K -n $NS get pods -l app=pt-deploy --field-selector status.phase=Running -o jsonpath='{.items[0].metadata.name}' 2>/dev/null); [ -z "$p" ] && { sleep 5; i=$((i + 5)); }; done
n=$($K -n $NS get pod "$p" -o jsonpath='{.spec.nodeName}' 2>/dev/null)
[ -n "$p" ] && [ "$n" = "$NODE2" ] && ok "PRECONDITION: $p Running on $NODE2 and reading ($(inpod_out "$p" cat /mnt/s3/shard-05.txt))" || bad "PRECONDITION: the Deployment's pod is '${p:-absent}' on '${n:-?}', not Running on $NODE2"
ip2=$($K get node "$NODE2" -o jsonpath='{.status.addresses[?(@.type=="InternalIP")].address}')
iid2=$($AWSR ec2 describe-instances --filters "Name=private-ip-address,Values=$ip2" --query 'Reservations[0].Instances[0].InstanceId' --output text 2>/dev/null)
[ -n "$iid2" ] && [ "$iid2" != "None" ] && ok "PRECONDITION: $NODE2 is instance $iid2" || bad "PRECONDITION: cannot resolve $NODE2's instance"
t0=$(now); $AWSR ec2 terminate-instances --instance-ids "$iid2" >/dev/null 2>&1 && note "terminated $iid2 at $(date -u +%T)"
i=0; while [ $i -lt 300 ] && node_ready "$NODE2"; do sleep 5; i=$((i + 5)); done
node_ready "$NODE2" && bad "$NODE2 still Ready 300s after termination" || ok "$NODE2 went NotReady ${i}s after termination"
i=0; p2=""; while [ $i -lt 600 ]; do p2=$($K -n $NS get pods -l app=pt-deploy -o json 2>/dev/null | jq -r '.items[] | select(.status.phase=="Running") | select(.spec.nodeName=="'"$NODE"'") | .metadata.name' | head -1); [ -n "$p2" ] && break; sleep 10; i=$((i + 10)); done
[ -n "$p2" ] && ok "the tenant rescheduled to $NODE as $p2 and is Running ($(( $(now) - t0 ))s after termination)" || bad "no replacement pod Running on $NODE 600s after the node was lost: $($K -n $NS get pods -l app=pt-deploy --no-headers | tr '\n' ';')"
[ -n "$p2" ] && { [ "$(inpod_out "$p2" cat /mnt/s3/shard-05.txt)" = "seeded-object-05" ] && ok "the rescheduled tenant reads its mount" || bad "the rescheduled tenant cannot read"; }
$K -n $NS delete deploy pt-deploy --wait=false >/dev/null 2>&1

# ── roster ────────────────────────────────────────────────────────────
echo
for want in P1 P2 P3 P4 P5 P6 P7 P8 P9 P10 P11 P12 P13 P14; do echo " $RAN_LEGS " | grep -q " $want " || bad "leg $want never ran"; done
echo "════════════════════════════════════════"
echo "flint-passthrough on real nodes: $PASS ok, $FAILED bad, $SKIPPED skipped"
[ "$FAILED" = "0" ]
