#!/usr/bin/env bash
# Read-only and read-write mounts on real nodes, side by side, under each
# way the broker can hold a reader to reads (per-user access design §10.5,
# phase F). Runs AFTER `run-s3csi.sh setup` (STORE=s3 NODE_EXEC=nodesh)
# against the bucket and identities `aws-access-iam.sh up` made.
#
# THREE ARMS, one broker backend each (helm upgrade between them):
#
#   A  sessionPolicy — backend sts, answered by the STS stand-in
#      (sts-shim.py): the broker's session policy goes to AWS STS
#      AssumeRole verbatim, on a role whose own policy is bucket-wide.
#      The mixed legs run here, because this is the arm AWS enforces with
#      the policy flint BUILDS.
#   B  readKey — backend static with a read key set (a read-only IAM user).
#   C  cooperative — backend static without one: the chart's default. The
#      mount and the syncer hold a reader; the bucket does not, and the
#      drill checks that the broker SAYS so rather than pretending.
#
# Legs (arm A unless named):
#   A0  the backend is what the broker reports, and the stand-in answers
#   A1  a mixed lean workspace: two writers, a reader by ServiceAccount, a
#       reader by csi.readOnly; two of them on each worker. Both readers ask
#       readOnly: true — on lean a read-only ServiceAccount must (A1b)
#   A1b a read-only ServiceAccount asking readOnly: false is REFUSED on lean,
#       with the fix named (run 2 measured why: the container runtime
#       remounts the volume with the pod's rw, so no plugin bind can narrow it)
#   A2  the presentation: writers write, readers get EROFS (tree and .flint/),
#       and the HOST's copy of each tenant bind — the one kubelet gives the
#       container — is ro for a reader. The first run read only the
#       container, where kubelet's own readOnly hid a plugin bind that was
#       rw on the host (fixed in fuse::bind_mount).
#   A3  the syncer's mode (FLINT_SYNC_ACCESS, capabilities.json)
#   A4  the broker's own record: access, enforcement, on_behalf_of per pod
#   A5  the stand-in's record: a Policy on every reader's exchange, none on
#       a writer's — independent of the broker's account
#   A6  F3/F8 with the keys the readers HOLD: PUT/DELETE under the prefix
#       and GET/LIST of another prefix denied; the writer's keys allowed
#   A7  convergence: each writer's publish reaches both readers and the other
#       writer; a delete reaches the readers
#   A8  readers took no fence and published nothing (their syncer logs)
#   A9  passthrough, mixed: a writer writes through mount-s3, a reader reads
#       it through mount-s3 under the session policy, and cannot write
#   A10 precedence: serviceAccounts ["*"] with a NAMED read-only entry
#   A11 an SA in neither list is refused, and the event names both lists
#   A12 a CR narrowed while its writer runs: the next mint is a read grant
#       and the writer's next publish lands nothing
#   A13 the same mixed shape on a workspace with NO ceiling (sizeLimitGib 0):
#       a plain directory tree, no loop device under the bind
#   B1  readKey: a reader holds the read user's key, which cannot write
#   C1  cooperative: the broker says so, the reader is still read-only in
#       its mount and syncer, and its key CAN write (the honest limit)
#
# Env: as run-s3csi.sh (KUBECONFIG CTX TAG BUCKET S3_REGION S3_KEY_FILE),
# plus RO_KEY_FILE STS_KEY_FILE ROLE_ARN (aws-access-iam.sh env), and OUT
# (evidence directory; default results/access-<date>).
set -u
cd "$(dirname "$0")"
export STORE=s3 NODE_EXEC=nodesh
REPO=$(cd ../.. && pwd)
eval "$(sed -n '/^CTX=\${CTX:-/,/^# ── setup \/ teardown/p' run-s3csi.sh | sed '$d')"
eval "$(sed -n '/^lobj()   {/,/^lmhas()  {/p' run-s3csi.sh)"
for v in RO_KEY_FILE STS_KEY_FILE ROLE_ARN; do [ -n "${!v:-}" ] || { echo "$v is required" >&2; exit 2; }; done
OUT=${OUT:-$REPO/s3csi/e2e/results/access-$(date +%Y-%m-%d)}
mkdir -p "$OUT"
ARMS=${ARMS:-A B C}
NARROW_WAIT=${NARROW_WAIT:-1200}
CP=$($K get nodes -l node-role.kubernetes.io/control-plane -o jsonpath='{.items[0].metadata.name}' 2>/dev/null)
NODE2=$($K get nodes -l '!node-role.kubernetes.io/control-plane' -o jsonpath='{.items[1].metadata.name}' 2>/dev/null)
[ -n "$NODE2" ] || NODE2=$NODE
SKIPPED=0

# ── helpers ───────────────────────────────────────────────────────────
wenvv() { $K -n $WNS exec "$1" -- sh -c "for p in /proc/[0-9]*; do tr '\\0' '\\n' < \$p/environ 2>/dev/null | sed -n 's/^$2=//p'; done | head -1" 2>/dev/null; }
poddel() { $K -n $NS delete pod "$@" --ignore-not-found --wait=true --timeout=300s >/dev/null 2>&1; }
# accpod NAME SA NODE workspace|mount CR true|false [on-behalf-of]
accpod() {
    local obo="#"
    [ -n "${7:-}" ] && obo="chert.us/on-behalf-of: $7"
    sed -e "s#__NAME__#$1#g" -e "s#__SA__#$2#g" -e "s#__NODE__#$3#g" -e "s#__SELKEY__#$4#g" \
        -e "s#__CR__#$5#g" -e "s#__RO__#$6#g" -e "s|__OBO__|$obo|g" acc-pod.yaml.tpl | $K apply -f - >/dev/null
}
tenants() { sed -e "s#__B__#$BUCKET#g" -e "s#__ENDPOINT__#$S3_ENDPOINT#g" -e "s#__REGION__#$S3_REGION#g" acc-tenants.yaml.tpl; }
pod_uid() { $K -n $NS get pod "$1" -o jsonpath='{.metadata.uid}' 2>/dev/null; }
broker_pod() { $K -n $SYS get pods -l app.kubernetes.io/name=flint-s3-broker --field-selector=status.phase=Running -o jsonpath='{.items[0].metadata.name}' 2>/dev/null; }
broker_log() { $K -n $SYS logs "$(broker_pod)" 2>/dev/null | sed 's/\x1b\[[0-9;]*m//g'; }
# The LAST `issued` line for a pod uid, as "access enforcement on_behalf_of".
issued_of() {
    broker_log | python3 -c "
import re, sys
uid = '$1'; last = None
for line in sys.stdin:
    if ' issued' not in line or ('pod_uid=Some(\"%s\")' % uid) not in line:
        continue
    f = lambda k: (re.search(k + r'=\"?([^\" ]*)\"?', line) or [None, '-'])[1]
    obo = re.search(r'on_behalf_of=Some\(\"([^\"]*)\"\)', line)
    last = '%s %s %s' % (f('access'), f('enforcement'), obo.group(1) if obo else '-')
print(last or '')"
}
# Poll until a pod's latest issued line is "$2", bounded by $3 s; prints it.
wait_issued() {
    local u i=0 got=""; u=$(pod_uid "$1")
    while [ $i -lt "$3" ]; do got=$(issued_of "$u"); [ "$got" = "$2" ] && break; sleep 5; i=$((i + 5)); done
    printf '%s' "$got"
}
# The HOST's per-mount flags and source for a tenant's volume target, as
# "<flags> <source>": what kubelet hands the container, not the plugin's view.
host_bind() {
    local u n; u=$(pod_uid "$1"); n=$($K -n $NS get pod "$1" -o jsonpath='{.spec.nodeName}' 2>/dev/null)
    "$REPO/scripts/nodesh.sh" "$n" "grep '$u/volumes/kubernetes.io~csi/ws/mount ' /proc/self/mountinfo | head -1 | awk '{for (i=1;i<=NF;i++) if (\$i==\"-\") {print \$6, \$(i+2); exit}}'" 2>/dev/null | tail -1
}
count_issued_of() { broker_log | grep ' issued' | grep -c "pod_uid=Some(\"$1\")"; }
broker_status() {
    local p="accstatus-$RANDOM" i=0
    $K -n $SYS run "$p" --restart=Never --image=busybox:1.36 -- wget -q -O - "http://flint-s3-broker.$SYS.svc/v1/status" >/dev/null 2>&1
    while [ $i -lt 60 ] && [ "$($K -n $SYS get pod "$p" -o jsonpath='{.status.phase}' 2>/dev/null)" != "Succeeded" ]; do sleep 2; i=$((i + 2)); done
    $K -n $SYS logs "$p" 2>/dev/null
    $K -n $SYS delete pod "$p" --wait=false >/dev/null 2>&1
}
wait_worker() { local w="" i=0; while [ $i -lt "${2:-300}" ]; do w=$(worker_of "$1"); [ -n "$w" ] && break; sleep 5; i=$((i + 5)); done; printf '%s' "$w"; }
# The keys a worker holds, as three words (AK SK TOKEN|-). Never echoed.
wcreds() {
    $K -n $WNS exec "$1" -- cat /comm/creds.json 2>/dev/null | python3 -c "
import json, sys
d = json.load(sys.stdin)
print(d['AccessKeyId'], d['SecretAccessKey'], d.get('Token') or '-')" 2>/dev/null
}
wnonce() { $K -n $WNS exec "$1" -- cat /comm/auth.token 2>/dev/null; }
# awsx "AK SK TOKEN" <aws args...> — from the CP's aws-cli pod, output to stdout+stderr.
awsx() {
    local ak sk tok; set -- $1 "${@:2}"; ak=$1; sk=$2; tok=$3; shift 3
    if [ "$tok" = "-" ]; then
        $K -n $SYS exec awscli -- env -u AWS_SESSION_TOKEN AWS_ACCESS_KEY_ID="$ak" AWS_SECRET_ACCESS_KEY="$sk" AWS_DEFAULT_REGION="$S3_REGION" aws "$@" 2>&1
    else
        $K -n $SYS exec awscli -- env AWS_ACCESS_KEY_ID="$ak" AWS_SECRET_ACCESS_KEY="$sk" AWS_SESSION_TOKEN="$tok" AWS_DEFAULT_REGION="$S3_REGION" aws "$@" 2>&1
    fi
}
# A write in the tenant: prints "ok" or the shell's error.
# kubectl's own "command terminated with exit code 1" is dropped, or it
# would be the last line instead of the shell's error.
twrite() { $K -n $NS exec "$1" -c agent -- sh -c "mkdir -p \$(dirname $2) 2>&1 && printf '%s' '$3' > $2 2>&1 && echo ok" 2>&1 | grep -v '^command terminated' | tail -1; }
tcat() { $K -n $NS exec "$1" -c agent -- cat "$2" 2>/dev/null; }
# "gone" only when the exec ran and the path is absent: an empty cat is also
# what a failed exec returns.
tgone() { [ "$($K -n $NS exec "$1" -c agent -- sh -c "test -e $2 && echo present || echo gone" 2>/dev/null)" = gone ]; }
wait_gone() { local i=0; while [ $i -lt "$3" ]; do tgone "$1" "$2" && return 0; sleep 3; i=$((i + 3)); done; return 1; }
# A worker's key expiry, as epoch seconds.
wexp() { $K -n $WNS exec "$1" -- cat /comm/creds.json 2>/dev/null | python3 -c "
import datetime, json, sys
e = json.load(sys.stdin)['Expiration']
print(int(datetime.datetime.fromisoformat(e.replace('Z', '+00:00')).timestamp()))" 2>/dev/null; }
# Declare a boundary and wait for its ack; prints the ack's status.
tpublish() { # pod nonce secs
    $K -n $NS exec "$1" -c agent -- sh -c "
        printf '{\"nonce\":\"$2\"}' > /workspace/.flint/.publish.tmp && mv /workspace/.flint/.publish.tmp /workspace/.flint/publish || exit 1
        n=0; while [ \$n -lt $3 ]; do grep -qs '\"$2\"' /workspace/.flint/publish.ack && break; n=\$((n + 1)); sleep 1; done
        grep -s '\"$2\"' /workspace/.flint/publish.ack >/dev/null || { echo no-ack; exit 0; }
        tr -d '\n' < /workspace/.flint/publish.ack | sed -n 's/.*\"status\": *\"\([^\"]*\)\".*/\1/p'" 2>/dev/null
}
# Wait until a tenant file has the content, or is gone ("") — bounded.
wait_content() { # pod path want secs
    local i=0 got
    while [ $i -lt "$4" ]; do
        got=$(tcat "$1" "$2")
        [ "$got" = "$3" ] && return 0
        sleep 3; i=$((i + 3))
    done
    return 1
}
helm_broker() { # extra --set args
    helm --kube-context "$CTX" upgrade --install flint-s3-csi "$REPO/flint-s3-csi-chart" -n $SYS \
        --set node.image.tag="$TAG" --set workers.passthroughImage.tag="$TAG" --set workers.leanImage.tag="$TAG" \
        --set node.image.pullPolicy=IfNotPresent \
        --set node.credsLifetimeSecs="$CREDS_LIFETIME" --set broker.replicas=1 \
        --set node.region="$S3_REGION" \
        --set node.logLevel=debug --set broker.logLevel=debug "$@" >/dev/null || return 1
    $K -n $SYS rollout status deploy/flint-s3-broker --timeout=180s >/dev/null
    # `rollout status` returns while the previous pod is still TERMINATING,
    # and still answering: the first run's pods on the second node were
    # minted by the outgoing static broker (never reached the stand-in, held
    # an unscoped key). Wait for one broker pod, then for every node's
    # service proxy to drop the old endpoint.
    local i=0
    while [ $i -lt 180 ] && [ "$($K -n $SYS get pods -l app.kubernetes.io/name=flint-s3-broker --no-headers 2>/dev/null | wc -l | tr -d ' ')" != 1 ]; do
        sleep 2; i=$((i + 2))
    done
    sleep 20
}
save() { "$@" > "$OUT/$SAVE_AS" 2>&1; }
evidence() { # tag
    broker_log > "$OUT/$1-broker.log"
    $K -n $SYS logs deploy/sts-shim > "$OUT/$1-shim.log" 2>/dev/null
    for p in $($K -n $NS get pods -l suite=acc -o jsonpath='{.items[*].metadata.name}' 2>/dev/null); do
        w=$(worker_of "$p"); [ -n "$w" ] && $K -n $WNS logs "$w" > "$OUT/$1-worker-$p.log" 2>/dev/null
    done
}
clear_pods() { $K -n $NS delete pods -l suite=acc --ignore-not-found --wait=true --timeout=600s >/dev/null 2>&1; }

echo "flint read-only/read-write mounts on real nodes — cluster $CTX, nodes $NODE + $NODE2, CP $CP, bucket $BUCKET, arms: $ARMS"
echo "evidence: $OUT"
$K get csidriver s3.csi.chert.us >/dev/null 2>&1 || { echo "no s3.csi.chert.us — run run-s3csi.sh setup first"; exit 2; }
img=$($K -n $SYS get ds flint-s3-csi-node -o jsonpath='{.spec.template.spec.containers[0].image}')
echo "$img" | grep -q ":$TAG\$" || { echo "the plugin runs $img, not tag $TAG"; exit 2; }
tenants | $K apply -f - >/dev/null || { echo "the access tenants were refused"; exit 2; }
STS_AK=$(jq -r .AccessKey.AccessKeyId "$STS_KEY_FILE"); STS_SK=$(jq -r .AccessKey.SecretAccessKey "$STS_KEY_FILE")
RO_AK=$(jq -r .AccessKey.AccessKeyId "$RO_KEY_FILE"); RO_SK=$(jq -r .AccessKey.SecretAccessKey "$RO_KEY_FILE")
$K -n $SYS create configmap sts-shim-script --from-file=sts-shim.py=sts-shim.py --dry-run=client -o yaml | $K apply -f - >/dev/null
sed -e "s#__REGION__#$S3_REGION#g" -e "s#__ROLE_ARN__#$ROLE_ARN#g" -e "s#__STS_KEY__#$STS_AK#g" \
    -e "s#__STS_SECRET__#$STS_SK#g" -e "s#__CP__#$CP#g" sts-shim.yaml.tpl | $K apply -f - >/dev/null
$K -n $SYS create secret generic s3-broker-static-read --from-literal=AWS_ACCESS_KEY_ID="$RO_AK" \
    --from-literal=AWS_SECRET_ACCESS_KEY="$RO_SK" --dry-run=client -o yaml | $K apply -f - >/dev/null
$K -n $SYS rollout status deploy/sts-shim --timeout=180s >/dev/null || echo "  NOTE: sts-shim not ready"
$K -n $SYS wait --for=condition=ready pod/awscli --timeout=180s >/dev/null || echo "  NOTE: awscli pod not ready"
RW_AK=$AWS_ACCESS_KEY_ID; RW_KEYS="$AWS_ACCESS_KEY_ID $AWS_SECRET_ACCESS_KEY -"
# A seeded object in ANOTHER prefix, which no workspace key may read.
mcx sh -c "printf 'not-for-readers\n' | mc pipe m/$BUCKET/private/access-secret.txt" >/dev/null 2>&1
[ "$(awsx "$RW_KEYS" s3 cp "s3://$BUCKET/private/access-secret.txt" - 2>/dev/null)" = "not-for-readers" ] \
    && ok "PRECONDITION: the other-prefix fixture exists and the rig's key reads it (the aws-cli pod works)" \
    || bad "PRECONDITION: cannot read the other-prefix fixture with the rig's key — every denial below would be vacuous"

# AccessDenied from a request with a body; a bare 403 Forbidden from a HEAD
# (`aws s3 cp` HEADs first), which run 2 misread as "not denied".
denied() { echo "$1" | grep -qE 'AccessDenied|\(403\)'; }

if echo " $ARMS " | grep -q " A "; then
# ══ ARM A: session policy ═════════════════════════════════════════════
leg A0 "backend sts, answered by the STS stand-in: the broker reports sessionPolicy"
clear_pods
helm_broker --set broker.backend=sts --set broker.sts.url=http://sts-shim.$SYS.svc:8080/ --set broker.sts.roleArn="$ROLE_ARN" \
    || bad "helm upgrade to the sts backend failed"
st=$(broker_status)
echo "$st" | grep -q '"backend":"sts"' && echo "$st" | grep -q '"readEnforcement":"sessionPolicy"' \
    && ok "broker /v1/status: backend sts, readEnforcement sessionPolicy" || bad "broker status: $st"

# The narrowing leg's writer starts first: its wait is the longest.
leg A12 "(start) a writer on acc-narrow publishes while its SA is still read-write"
$K -n $NS patch flintleanworkspace acc-narrow --type merge -p '{"spec":{"consumers":{"serviceAccounts":["editor"],"readOnlyServiceAccounts":[]}}}' >/dev/null
accpod nw editor "$NODE" workspace acc-narrow false
NARROW_READY=0
if wait_phase nw Running 300; then
    [ "$(twrite nw /workspace/before.txt before-narrowing)" = ok ] && [ "$(tpublish nw narrow-before 90)" = ok ] \
        && { ok "CONTROL: nw's publish before the narrowing is acked ok"; NARROW_READY=1; } \
        || bad "nw could not publish before the narrowing — the leg would prove nothing"
    [ $NARROW_READY = 1 ] && lmhas access/narrow before.txt && ok "CONTROL: before.txt is in acc-narrow's manifest" || bad "before.txt is not in the manifest"
    NW_UID=$(pod_uid nw); NW_ISSUED0=$(count_issued_of "$NW_UID"); NW_W=$(wait_worker nw)
    $K -n $NS patch flintleanworkspace acc-narrow --type merge -p '{"spec":{"consumers":{"serviceAccounts":[],"readOnlyServiceAccounts":["editor"]}}}' >/dev/null \
        && note "acc-narrow now lists editor as read-only; nw's keys change at its next mint (issued so far: $NW_ISSUED0)"
    NW_AK0=$(wcreds "$NW_W" | cut -d' ' -f1); NW_EXP0=$(wexp "$NW_W")
    NARROW_T0=$(date +%s)
else
    bad "nw is not Running: $(mount_events nw | tail -1 | cut -c1-200)"
fi

leg A1 "a mixed lean workspace: writers w1 ($NODE) and w2 ($NODE2), reader r-sa by SA ($NODE), reader r-flag by csi.readOnly ($NODE2)"
accpod w1 editor "$NODE" workspace acc-lean false
accpod w2 editor "$NODE2" workspace acc-lean false
accpod r-sa viewer "$NODE" workspace acc-lean true alice@example.com
accpod r-flag editor "$NODE2" workspace acc-lean true
allup=1
for p in w1 w2 r-sa r-flag; do
    if wait_phase "$p" Running 420; then ok "$p is Running"; else bad "$p is not Running: $(mount_events "$p" | tail -1 | cut -c1-240)"; allup=0; fi
done
# "none" rather than empty: these are word-split into `set --` below, where
# an empty name would shift every later field into its place.
W1=$(wait_worker w1); W2=$(wait_worker w2); RSA=$(wait_worker r-sa); RFL=$(wait_worker r-flag)
W1=${W1:-none}; W2=${W2:-none}; RSA=${RSA:-none}; RFL=${RFL:-none}

leg A1b "a read-only ServiceAccount asking readOnly: false on lean is refused, naming the fix"
accpod r-rw viewer "$NODE" workspace acc-lean false
i=0; ev=""; while [ $i -lt 120 ]; do ev=$(mount_events r-rw); echo "$ev" | grep -q 'readOnly: true' && break; sleep 5; i=$((i + 5)); done
[ "$($K -n $NS get pod r-rw -o jsonpath='{.status.phase}')" != Running ] && echo "$ev" | grep -q "set \`readOnly: true\`" \
    && ok "r-rw stays out, and its event says to set readOnly: true" || bad "r-rw: phase $($K -n $NS get pod r-rw -o jsonpath='{.status.phase}'), event: $(echo "$ev" | tail -1 | cut -c1-240)"
[ -z "$(worker_of_any r-rw)" ] && ok "no worker was created for r-rw (refused before anything was built)" || bad "a worker exists for the refused r-rw"
poddel r-rw

leg A2 "the presentation: writers write; readers get EROFS in the tree and in .flint/"
for p in w1 w2; do
    r=$(twrite "$p" "/workspace/probe-$p.txt" "$p"); [ "$r" = ok ] && ok "$p writes its tree" || bad "$p cannot write: $r"
done
for p in r-sa r-flag; do
    r=$(twrite "$p" "/workspace/probe-$p.txt" "$p")
    echo "$r" | grep -qi 'read-only file system' && ok "$p: write refused with EROFS ($r)" || bad "$p write was not EROFS: '$r'"
    r=$(twrite "$p" /workspace/.flint/sync x)
    echo "$r" | grep -qi 'read-only file system' && ok "$p: .flint/sync refused with EROFS too (D11)" || bad "$p .flint/sync: '$r'"
done

for pe in "w1 rw" "w2 rw" "r-sa ro" "r-flag ro"; do
    set -- $pe
    cf=$($K -n $NS exec "$1" -c agent -- grep ' /workspace ' /proc/self/mountinfo 2>/dev/null | awk '{print $6}')
    case "$cf" in "$2",*) ok "$1: the container's own /workspace is $2 ($cf)" ;; *) bad "$1: the container's /workspace is '$cf', want $2" ;; esac
    hb=$(host_bind "$1")
    case "$hb" in
        "$2",*) ok "$1: the HOST's copy of its bind is $2 ($hb)" ;;
        *) bad "$1: the HOST's copy of its bind is '$hb', want $2" ;;
    esac
done

leg A3 "the syncer's mode: FLINT_SYNC_ACCESS and capabilities.json"
for pw in "w1 $W1 readWrite" "w2 $W2 readWrite" "r-sa $RSA read" "r-flag $RFL read"; do
    set -- $pw
    [ "$2" != none ] || { bad "$1 has no worker"; continue; }
    env_access=$(wenvv "$2" FLINT_SYNC_ACCESS)
    cap=$(tcat "$1" /workspace/.flint/capabilities.json | tr -d ' \n' | sed -n 's/.*"access":"\([^"]*\)".*/\1/p')
    if [ "$3" = read ]; then
        [ "$env_access" = read ] && ok "$1: syncer launched with FLINT_SYNC_ACCESS=read" || bad "$1: FLINT_SYNC_ACCESS='$env_access'"
    else
        [ -z "$env_access" ] && ok "$1: syncer launched with no access line (read-write)" || bad "$1: FLINT_SYNC_ACCESS='$env_access'"
    fi
    [ "$cap" = "$3" ] && ok "$1: capabilities.json says access $3" || bad "$1: capabilities.json access '$cap'"
done

leg A4 "the broker's record: a read grant with a session policy for each reader, a write grant for each writer"
for pe in "w1 readWrite none -" "w2 readWrite none -" "r-sa read sessionPolicy alice@example.com" "r-flag read sessionPolicy -"; do
    set -- $pe
    got=$(wait_issued "$1" "$2 $3 $4" 150)
    [ "$got" = "$2 $3 $4" ] && ok "$1: issued access=$2 enforcement=$3 on_behalf_of=$4" || bad "$1: issued '$got', want '$2 $3 $4'"
done

leg A5 "the stand-in's record: AWS was asked for a policy on every reader's exchange and on no writer's"
$K -n $SYS logs deploy/sts-shim > "$OUT/A-shim.log" 2>/dev/null
for pw in "w1 $W1 false" "w2 $W2 false" "r-sa $RSA true" "r-flag $RFL true"; do
    set -- $pw
    n=$(wnonce "${2:-none}")
    [ -n "$n" ] || { bad "$1: no nonce in its worker's comm dir"; continue; }
    lines=$(grep "\"session\": \"$n\"" "$OUT/A-shim.log")
    [ -n "$lines" ] || { bad "$1: the stand-in never saw session $n"; continue; }
    if echo "$lines" | grep -q "\"policy_present\": $3" && ! echo "$lines" | grep -q "\"policy_present\": $([ "$3" = true ] && echo false || echo true)"; then
        ok "$1: every exchange of session $n carried policy_present=$3 ($(echo "$lines" | wc -l | tr -d ' ') exchange(s), all status $(echo "$lines" | sed -n 's/.*"status": \([0-9]*\).*/\1/p' | sort -u | tr '\n' ' '))"
    else
        bad "$1: stand-in lines for $n: $(echo "$lines" | tail -2)"
    fi
done
# The policy itself, as AWS received it, must be the one the unit test pins.
want=$(cd "$REPO/spdk-csi-driver" && CARGO_INCREMENTAL=0 FLINT_S3B_WRITE_READ_POLICY="$OUT/expected-policy.json" \
    FLINT_S3B_READ_POLICY_TARGET="$BUCKET/access/lean" cargo test -q --lib the_read_session_policy_reads_the_prefix_and_nothing_else >/dev/null 2>&1; \
    shasum -a 256 "$OUT/expected-policy.json" 2>/dev/null | cut -d' ' -f1)
n=$(wnonce "${RSA:-none}")
got=$(grep "\"session\": \"$n\"" "$OUT/A-shim.log" | tail -1 | sed -n 's/.*"policy_sha256": "\([0-9a-f]*\)".*/\1/p')
[ -n "$want" ] && [ "$want" = "$got" ] && ok "r-sa's policy as AWS received it is byte-identical to read_session_policy(aws, $BUCKET, access/lean)" \
    || bad "policy hash: stand-in saw '$got', the unit test writes '$want'"

leg A6 "F3/F8 with the keys each pod HOLDS, evaluated by AWS"
WK=$(wcreds "${W1:-none}")
# A sacrificial object for the DELETE probe: a denial that turned out to be
# a permission would delete this, never the workspace's own metadata.
awsx "$RW_KEYS" s3api put-object --bucket "$BUCKET" --key access/lean/_drill/keep.txt --body /etc/hostname >/dev/null
for p in r-sa r-flag; do
    w=$([ $p = r-sa ] && echo "$RSA" || echo "$RFL")
    RK=$(wcreds "${w:-none}")
    [ -n "$RK" ] || { bad "$p: no creds.json in its worker"; continue; }
    out=$(awsx "$RK" s3api put-object --bucket "$BUCKET" --key "access/lean/_drill/$p-put.txt" --body /etc/hostname)
    denied "$out" && ok "$p's keys: PUT under the prefix → AccessDenied" || bad "$p's keys PUT: $(echo "$out" | tail -1)"
    out=$(awsx "$RK" s3api delete-object --bucket "$BUCKET" --key access/lean/_drill/keep.txt)
    denied "$out" && ok "$p's keys: DELETE under the prefix → AccessDenied" || bad "$p's keys DELETE: $(echo "$out" | tail -1)"
    out=$(awsx "$RK" s3 cp "s3://$BUCKET/access/lean/.flint/lean/current" -)
    echo "$out" | grep -q '"seq"' && ok "$p's keys: GET of its own prefix reads the pointer" || bad "$p's keys GET own prefix: $(echo "$out" | tail -1)"
    out=$(awsx "$RK" s3 cp "s3://$BUCKET/private/access-secret.txt" -)
    denied "$out" && ok "$p's keys: GET of another prefix → AccessDenied (F8)" || bad "$p's keys GET other prefix: $(echo "$out" | tail -1)"
    out=$(awsx "$RK" s3api list-objects-v2 --bucket "$BUCKET" --prefix private/)
    denied "$out" && ok "$p's keys: LIST of another prefix → AccessDenied" || bad "$p's keys LIST other prefix: $(echo "$out" | tail -1)"
done
if [ -n "$WK" ]; then
    out=$(awsx "$WK" s3api put-object --bucket "$BUCKET" --key access/lean/_drill/w1-put.txt --body /etc/hostname)
    echo "$out" | grep -q ETag && ok "CONTROL: w1's keys PUT under the prefix" || bad "CONTROL: w1's keys cannot PUT: $(echo "$out" | tail -1)"
    awsx "$WK" s3api delete-object --bucket "$BUCKET" --key access/lean/_drill/w1-put.txt >/dev/null
    out=$(awsx "$WK" s3 cp "s3://$BUCKET/private/access-secret.txt" -)
    [ "$out" = "not-for-readers" ] && ok "CONTROL: w1's keys (the role, no policy) read the other prefix — the narrowing above is the session policy's" \
        || bad "CONTROL: w1's keys cannot read the other prefix: $(echo "$out" | tail -1)"
else
    bad "w1: no creds.json in its worker"
fi

leg A7 "convergence: writers' publishes reach both readers and the other writer; a delete reaches the readers"
t=$(date +%s)
[ "$(twrite w1 /workspace/shared/from-w1.txt "w1-$t")" = ok ] || bad "w1 could not write shared/from-w1.txt"
s=$(tpublish w1 "w1-a-$t" 120); [ "$s" = ok ] && ok "w1's publish acked ok" || bad "w1's publish: '$s'"
[ "$(twrite w2 /workspace/shared/from-w2.txt "w2-$t")" = ok ] || bad "w2 could not write shared/from-w2.txt"
s=$(tpublish w2 "w2-a-$t" 120); [ "$s" = ok ] && ok "w2's publish acked ok" || bad "w2's publish: '$s'"
for p in r-sa r-flag; do
    wait_content "$p" /workspace/shared/from-w1.txt "w1-$t" 120 && ok "$p has w1's file" || bad "$p never got w1's file"
    wait_content "$p" /workspace/shared/from-w2.txt "w2-$t" 120 && ok "$p has w2's file" || bad "$p never got w2's file"
done
wait_content w2 /workspace/shared/from-w1.txt "w1-$t" 120 && ok "w2 integrated w1's file at its boundary" || bad "w2 never got w1's file"
$K -n $NS exec w1 -c agent -- rm -f /workspace/shared/from-w1.txt
s1=$(tpublish w1 "w1-d1-$t" 120); sleep 2; s2=$(tpublish w1 "w1-d2-$t" 120)
[ "$s1" = ok ] && [ "$s2" = ok ] && ok "w1's two delete boundaries acked ok" || bad "w1's delete publishes: '$s1' '$s2'"
lmhas access/lean shared/from-w1.txt && bad "shared/from-w1.txt is still in the manifest" || ok "shared/from-w1.txt left the manifest"
for p in r-sa r-flag; do
    wait_gone "$p" /workspace/shared/from-w1.txt 120 && ok "$p: the delete reached it" || bad "$p still has from-w1.txt"
done

leg A8 "readers took no fence and published nothing"
for pw in "r-sa $RSA" "r-flag $RFL"; do
    set -- $pw
    l=$($K -n $WNS logs "${2:-none}" 2>/dev/null)
    echo "$l" | grep -q "read access" && ok "$1's syncer says read access" || bad "$1's syncer log has no read-access line"
    echo "$l" | grep -qiE "fence held|barrier seq=" && bad "$1's syncer held a fence or ran a barrier: $(echo "$l" | grep -iE 'fence held|barrier seq=' | head -1)" \
        || ok "$1's syncer never held the fence nor ran a barrier ($(echo "$l" | grep -c 'pull seq=') pulls)"
    echo "$l" | grep -qE "not authorized|AccessDenied|REFUSED reason=auth" && bad "$1's syncer was denied: $(echo "$l" | grep -E 'not authorized|AccessDenied|REFUSED' | head -1)" \
        || ok "$1's syncer was denied nothing on its read grant"
done
evidence A-lean

leg A9 "passthrough, mixed: pw writes through mount-s3, pr reads through mount-s3 under the session policy"
accpod pw editor "$NODE" mount acc-pt false
if wait_phase pw Running 420; then
    r=$(twrite pw /workspace/from-pw.txt "pw-$t"); [ "$r" = ok ] && ok "pw writes through mount-s3" || bad "pw write: $r"
    [ "$(lobj access/pt/from-pw.txt)" = "pw-$t" ] && ok "from-pw.txt is in the bucket" || bad "from-pw.txt is not in the bucket"
else
    bad "pw is not Running: $(mount_events pw | tail -1 | cut -c1-240)"
fi
accpod pr viewer "$NODE2" mount acc-pt false
if wait_phase pr Running 420; then
    PR=$(wait_worker pr)
    [ "$(tcat pr /workspace/from-pw.txt)" = "pw-$t" ] && ok "pr reads pw's file through mount-s3 on a read grant (list + get under the prefix condition)" \
        || bad "pr cannot read from-pw.txt: '$(tcat pr /workspace/from-pw.txt)' ls: $($K -n $NS exec pr -c agent -- ls /workspace 2>&1 | tr '\n' ' ')"
    r=$(twrite pr /workspace/from-pr.txt x)
    echo "$r" | grep -qiE 'read-only file system|permission denied|operation not permitted' && ok "pr's write refused ($r)" || bad "pr's write: '$r'"
    hb=$(host_bind pr); case "$hb" in ro,*) ok "pr: the HOST's copy of its bind is ro ($hb)" ;; *) bad "pr: host bind '$hb'" ;; esac
    args=$($K -n $WNS exec "${PR:-none}" -- sh -c 'for p in /proc/[0-9]*; do tr "\0" " " < $p/cmdline 2>/dev/null; echo; done' 2>/dev/null | grep -- '--prefix' | head -1)
    echo "$args" | grep -q -- '--read-only' && ok "pr's mount-s3 runs with --read-only" || bad "pr's mount-s3 args: $args"
    got=$(issued_of "$(pod_uid pr)"); [ "$got" = "read sessionPolicy -" ] && ok "pr: issued access=read enforcement=sessionPolicy" || bad "pr: issued '$got'"
    RK=$(wcreds "${PR:-none}")
    out=$(awsx "$RK" s3api put-object --bucket "$BUCKET" --key access/pt/pr-put.txt --body /etc/hostname)
    denied "$out" && ok "pr's keys: PUT → AccessDenied" || bad "pr's keys PUT: $(echo "$out" | tail -1)"
else
    bad "pr is not Running: $(mount_events pr | tail -1 | cut -c1-240)"
fi

leg A10 "precedence: serviceAccounts [\"*\"] with a NAMED read-only viewer"
# wv asks read-write: under "*" alone it would be published read-write, so
# the refusal IS the evidence that the named read-only entry won; wv2 asks
# read-only and is published as a reader.
accpod wv viewer "$NODE" workspace acc-wild false
accpod wv2 viewer "$NODE" workspace acc-wild true
accpod we editor "$NODE2" workspace acc-wild false
for p in wv2 we; do wait_phase "$p" Running 420 || bad "$p is not Running: $(mount_events "$p" | tail -1 | cut -c1-240)"; done
i=0; ev=""; while [ $i -lt 120 ]; do ev=$(mount_events wv); echo "$ev" | grep -q 'readOnly: true' && break; sleep 5; i=$((i + 5)); done
echo "$ev" | grep -q 'readOnly: true' && ok "wv (named read-only, under \"*\", asking rw): refused as a read-only consumer" || bad "wv: event '$(echo "$ev" | tail -1 | cut -c1-200)'"
r=$(twrite wv2 /workspace/x.txt x); echo "$r" | grep -qi 'read-only file system' && ok "wv2 (named read-only, asking ro): EROFS" || bad "wv2 write: '$r'"
r=$(twrite we /workspace/x.txt x); [ "$r" = ok ] && ok "we (only \"*\"): writes" || bad "we write: '$r'"
got=$(wait_issued wv2 "read sessionPolicy -" 150); [ "${got%% *}" = read ] && ok "wv2: issued a read grant" || bad "wv2: issued '$got'"
got=$(wait_issued we "readWrite none -" 150); [ "${got%% *}" = readWrite ] && ok "we: issued a write grant" || bad "we: issued '$got'"
poddel wv

leg A11 "an SA in neither list is refused, naming both lists"
accpod st stranger "$NODE" workspace acc-lean false
i=0; ev=""; while [ $i -lt 120 ]; do ev=$(mount_events st); echo "$ev" | grep -q readOnlyServiceAccounts && break; sleep 5; i=$((i + 5)); done
[ "$($K -n $NS get pod st -o jsonpath='{.status.phase}')" != Running ] && echo "$ev" | grep -q "neither spec.consumers.serviceAccounts nor" \
    && ok "st stays out, and its event names both lists" || bad "st: phase $($K -n $NS get pod st -o jsonpath='{.status.phase}'), event: $(echo "$ev" | tail -1 | cut -c1-240)"
poddel st

leg A13 "no ceiling: a writer and a reader on a plain-directory tree (sizeLimitGib 0, no loop device)"
accpod dw editor "$NODE2" workspace acc-direct false
accpod dr viewer "$NODE" workspace acc-direct true
for p in dw dr; do wait_phase "$p" Running 420 || bad "$p is not Running: $(mount_events "$p" | tail -1 | cut -c1-240)"; done
for pe in "dw rw" "dr ro"; do
    set -- $pe
    hb=$(host_bind "$1")
    case "$hb" in
        "$2",*) echo "$hb" | grep -q '/dev/loop' && bad "$1: the tree is on a loop device ($hb) — the leg is not testing a plain directory" \
                    || ok "$1: HOST bind $2 on a plain directory ($hb)" ;;
        *) bad "$1: the HOST's copy of its bind is '$hb', want $2" ;;
    esac
done
r=$(twrite dr /workspace/x.txt x); echo "$r" | grep -qi 'read-only file system' && ok "dr: EROFS" || bad "dr write: '$r'"
t=$(date +%s)
[ "$(twrite dw /workspace/from-dw.txt "dw-$t")" = ok ] && [ "$(tpublish dw "dw-$t" 120)" = ok ] && ok "dw published on the plain tree" || bad "dw could not publish"
wait_content dr /workspace/from-dw.txt "dw-$t" 120 && ok "dr follows dw" || bad "dr never got dw's file"
got=$(wait_issued dr "read sessionPolicy -" 150); [ "$got" = "read sessionPolicy -" ] && ok "dr: issued a read grant with a session policy" || bad "dr: issued '$got'"
evidence A-mixed

leg A12 "(finish) the narrowed CR: nw's next mint is a read grant, and its next publish lands nothing"
if [ "$NARROW_READY" = 1 ]; then
    i=0; got=""
    while [ $i -lt "$NARROW_WAIT" ]; do
        got=$(issued_of "$NW_UID"); [ "${got%% *}" = read ] && break
        sleep 15; i=$((i + 15))
    done
    el=$(( $(date +%s) - NARROW_T0 ))
    if [ "${got%% *}" = read ]; then
        ok "nw's volume, still registered read-write by the plugin, was minted a READ grant ${el}s after the narrowing (the CR narrows at the broker)"
        # The syncer's SDK keeps the key it cached until that key nears
        # expiry, so the new grant is not in use until the OLD key is dead.
        # Wait for the refreshed creds.json, then out the previous lifetime.
        i=0; while [ $i -lt 120 ]; do [ "$(wcreds "$NW_W" | cut -d' ' -f1)" != "$NW_AK0" ] && break; sleep 5; i=$((i + 5)); done
        [ "$(wcreds "$NW_W" | cut -d' ' -f1)" != "$NW_AK0" ] && ok "nw's worker now holds a different key" || bad "nw's creds.json still holds the pre-narrowing key"
        now=$(date +%s); [ "${NW_EXP0:-0}" -gt "$now" ] && { note "waiting $((NW_EXP0 - now + 30))s for the pre-narrowing key to expire"; sleep $((NW_EXP0 - now + 30)); }
        [ "$(twrite nw /workspace/after.txt after-narrowing)" = ok ] && ok "nw's tree is still writable (the bind is decided at publish; only the key changed)" || bad "nw could not write after.txt"
        s=$(tpublish nw narrow-after 120)
        [ "$s" != ok ] && ok "nw's publish after the narrowing is not acked ok ('$s')" || bad "nw's publish after the narrowing was acked ok"
        sleep 30
        lmhas access/narrow after.txt && bad "after.txt LANDED in acc-narrow's manifest on a read grant" || ok "after.txt is not in the manifest"
        [ -z "$(lobj access/narrow/files/after.txt)" ] && ok "and no object after.txt exists in the bucket" || note "an object named after.txt exists (an upload without a commit would be a finding)"
        l=$($K -n $WNS logs "$(worker_of nw)" 2>/dev/null)
        echo "$l" | grep -q "REFUSED reason=auth" && ok "nw's syncer calls it a credential refusal: $(echo "$l" | grep 'REFUSED reason=auth' | tail -1 | cut -c1-160)" \
            || bad "nw's syncer log has no auth refusal: $(echo "$l" | tail -2 | tr '\n' ' ' | cut -c1-240)"
    else
        bad "no read grant for nw within ${el}s (last issued: '$got')"
    fi
else
    skip "A12: the control publish never succeeded"
fi
evidence A-final
clear_pods
fi

if echo " $ARMS " | grep -q " B "; then
# ══ ARM B: static with a read key ═════════════════════════════════════
leg B1 "readKey: a reader holds the read user's key, which cannot write; a writer holds the one key"
helm_broker --set broker.backend=static --set broker.static.secretRef=s3-broker-static --set broker.static.readSecretRef=s3-broker-static-read \
    || bad "helm upgrade to static+read key failed"
st=$(broker_status); echo "$st" | grep -q '"readEnforcement":"readKey"' && ok "broker /v1/status: readEnforcement readKey" || bad "broker status: $st"
accpod bw editor "$NODE" workspace acc-lean false
accpod br viewer "$NODE2" workspace acc-lean true
for p in bw br; do wait_phase "$p" Running 420 || bad "$p is not Running: $(mount_events "$p" | tail -1 | cut -c1-240)"; done
BW=$(wait_worker bw); BR=$(wait_worker br)
set -- $(wcreds "${BR:-none}"); [ "${1:-}" = "$RO_AK" ] && ok "br holds the READ user's key" || bad "br holds ${1:0:4}…, not the read key"
set -- $(wcreds "${BW:-none}"); [ "${1:-}" = "$RW_AK" ] && ok "bw holds the one key" || bad "bw holds ${1:0:4}…"
out=$(awsx "$(wcreds "${BR:-none}")" s3api put-object --bucket "$BUCKET" --key access/lean/_drill/br-put.txt --body /etc/hostname)
denied "$out" && ok "br's keys: PUT → AccessDenied" || bad "br's keys PUT: $(echo "$out" | tail -1)"
got=$(wait_issued br "read readKey -" 150); [ "$got" = "read readKey -" ] && ok "br: issued access=read enforcement=readKey" || bad "br: issued '$got'"
t=$(date +%s)
[ "$(twrite bw /workspace/shared/from-bw.txt "bw-$t")" = ok ] && [ "$(tpublish bw "bw-$t" 120)" = ok ] || bad "bw could not publish"
wait_content br /workspace/shared/from-bw.txt "bw-$t" 120 && ok "br follows bw on the read key" || bad "br never got bw's file"
r=$(twrite br /workspace/x.txt x); echo "$r" | grep -qi 'read-only file system' && ok "br: EROFS" || bad "br write: '$r'"
hb=$(host_bind br); case "$hb" in ro,*) ok "br: the HOST's copy of its bind is ro" ;; *) bad "br: host bind '$hb'" ;; esac
evidence B
clear_pods
fi

if echo " $ARMS " | grep -q " C "; then
# ══ ARM C: static without a read key (the chart's default) ════════════
leg C1 "cooperative: the broker says so; the reader is read-only in its mount and syncer; its key CAN write"
helm_broker --set broker.backend=static --set broker.static.secretRef=s3-broker-static || bad "helm upgrade to plain static failed"
st=$(broker_status); echo "$st" | grep -q '"readEnforcement":"cooperative"' && ok "broker /v1/status: readEnforcement cooperative" || bad "broker status: $st"
broker_log | grep -q "no read key" && ok "the broker warned at start that a reader's key can write" || bad "no start-up warning in the broker log"
accpod cr viewer "$NODE" workspace acc-lean true
if wait_phase cr Running 420; then
    CRW=$(wait_worker cr)
    got=$(wait_issued cr "read cooperative -" 150); [ "$got" = "read cooperative -" ] && ok "cr: issued access=read enforcement=cooperative" || bad "cr: issued '$got'"
    r=$(twrite cr /workspace/x.txt x); echo "$r" | grep -qi 'read-only file system' && ok "cr: EROFS" || bad "cr write: '$r'"
    hb=$(host_bind cr); case "$hb" in ro,*) ok "cr: the HOST's copy of its bind is ro" ;; *) bad "cr: host bind '$hb'" ;; esac
    [ "$(wenvv "${CRW:-none}" FLINT_SYNC_ACCESS)" = read ] && ok "cr: the syncer is a reader" || bad "cr: syncer access '$(wenvv "${CRW:-none}" FLINT_SYNC_ACCESS)'"
    out=$(awsx "$(wcreds "${CRW:-none}")" s3api put-object --bucket "$BUCKET" --key access/lean/_drill/cr-put.txt --body /etc/hostname)
    if echo "$out" | grep -q ETag; then
        ok "cr's key CAN write (the documented limit of cooperative: only the mount and the syncer hold it)"
        awsx "$RW_KEYS" s3api delete-object --bucket "$BUCKET" --key access/lean/_drill/cr-put.txt >/dev/null
    else
        bad "cr's key could not write — then 'cooperative' is mislabelled: $(echo "$out" | tail -1)"
    fi
else
    bad "cr is not Running: $(mount_events cr | tail -1 | cut -c1-240)"
fi
evidence C
clear_pods
fi

echo
want_legs=""
echo " $ARMS " | grep -q " A " && want_legs="$want_legs A0 A1 A1b A2 A3 A4 A5 A6 A7 A8 A9 A10 A11 A12 A13"
echo " $ARMS " | grep -q " B " && want_legs="$want_legs B1"
echo " $ARMS " | grep -q " C " && want_legs="$want_legs C1"
for want in $want_legs; do echo " $RAN_LEGS " | grep -q " $want " || bad "leg $want never ran"; done
echo "════════════════════════════════════════"
echo "flint read-only/read-write mounts on real nodes: $PASS ok, $FAILED bad, $SKIPPED skipped"
[ "$FAILED" = "0" ]
