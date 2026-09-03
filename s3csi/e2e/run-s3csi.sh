#!/usr/bin/env bash
# s3.csi.chert.us end to end on kind — the CSI delivery of an S3 prefix into
# a tenant pod, with NO webhook, NO sidecar, NO credential and NO
# privilege in the tenant pod (docs/plans/csi-node-mount-design.md §10.2).
#
# House rules (passthrough/e2e, lean/e2e): every leg proves its
# precondition or FAILS; every refusal has an accepted control; readers
# run as uid 1001; every read asserts CONTENT; a same-bucket-other-prefix
# fixture falsifies the content legs; the roster fails the run if a leg
# never ran. A leg that could pass BECAUSE OF the defect it tests for is
# the failure mode this shape exists to prevent.
#
#   CTX=kind-flint-s3csi ./run-s3csi.sh setup      # MinIO + seed + chart + CRD + tenants
#   CTX=kind-flint-s3csi ./run-s3csi.sh            # the legs
#   CTX=kind-flint-s3csi ./run-s3csi.sh teardown
#
# Images: ./build-images.sh first (TAG=dev). Values overridable: TAG,
# CREDS_LIFETIME (seconds the plugin asks the broker for; short so the
# rotation leg is minutes, not hours).
set -u
cd "$(dirname "$0")"
REPO=$(cd ../.. && pwd)

CTX=${CTX:-kind-flint-s3csi}
TAG=${TAG:-dev}
CREDS_LIFETIME=${CREDS_LIFETIME:-120}
K="kubectl --context $CTX"
NS=s3-tenants
WNS=flint-workers
SYS=flint-system
NODE=$($K get nodes -o jsonpath='{.items[0].metadata.name}' 2>/dev/null)

PASS=0
FAILED=0
RAN_LEGS=""
bad()  { echo "  BAD: $1"; FAILED=$((FAILED + 1)); }
ok()   { PASS=$((PASS + 1)); echo "  ok: $1"; }
note() { echo "  NOTE: $1"; }
leg()  { RAN_LEGS="$RAN_LEGS $1"; echo; echo "── $1 — $2"; }

require_pod() {
    local ph
    ph=$($K -n $NS get pod "$1" -o jsonpath='{.status.phase}' 2>/dev/null)
    [ "$ph" = "Running" ] && return 0
    bad "pod $1 is '${ph:-absent}', not Running — every observation this leg makes would be an empty string"
    return 1
}
# exec can fail for a second or two after a pod turns Running (the
# container's streaming endpoint is not up yet): three tries, and only
# a non-zero EXIT is retried — a command that legitimately fails still
# fails, just 4 s later.
#
# NOTE the trailing newline: `$(...)` strips it anyway for a content
# comparison, but a caller that pipes this into `wc -l` counts NEWLINES,
# and a bare `printf %s` would silently under-count every listing by one
# (measured: an 11-entry mount read as 10, a 1-entry mount as 0).
inpod()  { local p=$1 try out rc; shift; for try in 1 2 3; do out=$($K -n $NS exec "$p" -c agent -- /bin/sh -c "$*" 2>/dev/null); rc=$?; [ $rc -eq 0 ] && break; sleep 2; done; [ -n "$out" ] && printf '%s\n' "$out"; return $rc; }
mcx()    { $K -n $SYS exec mc-s3 -- "$@" 2>/dev/null; }
onnode() { docker exec "$NODE" sh -c "$*" 2>/dev/null; }
# The worker pod serving a tenant pod, by annotation.
worker_of() {
    $K -n $WNS get pods -o json 2>/dev/null | python3 -c "
import json,sys
want='$NS/$1'
for p in json.load(sys.stdin)['items']:
    if p['metadata'].get('annotations',{}).get('chert.us/tenant-pod')==want and p.get('status',{}).get('phase')=='Running' and not p['metadata'].get('deletionTimestamp'):
        print(p['metadata']['name']); break"
}
# Wait for a pod phase, bounded.
wait_phase() { # pod phase secs
    local i=0
    while [ $i -lt "$3" ]; do
        [ "$($K -n $NS get pod "$1" -o jsonpath='{.status.phase}' 2>/dev/null)" = "$2" ] && return 0
        sleep 2; i=$((i + 2))
    done
    return 1
}
# Like inpod, but retried until the remote command produces OUTPUT: the
# probes below end in `|| true`, so a raced exec (the container's
# streaming endpoint is not up the instant the pod turns Running) comes
# back empty with exit 0 and would read as "the broker said nothing".
inpod_out() {
    local p=$1 try out; shift
    for try in 1 2 3 4 5; do
        out=$(inpod "$p" "$@")
        [ -n "$out" ] && break
        sleep 3
    done
    [ -n "$out" ] && printf '%s\n' "$out"
}
# The broker's issued counter, read by a throwaway pod whose LOGS are
# collected (never `kubectl run -i`: its attach drops output here).
broker_issued() {
    local p="brokerprobe-$RANDOM" i=0
    $K -n $SYS run "$p" --restart=Never --image=busybox:1.36 -- wget -q -O - "http://flint-s3-broker.$SYS.svc/v1/status" >/dev/null 2>&1
    while [ $i -lt 60 ] && [ "$($K -n $SYS get pod "$p" -o jsonpath='{.status.phase}' 2>/dev/null)" != "Succeeded" ]; do sleep 2; i=$((i + 2)); done
    $K -n $SYS logs "$p" 2>/dev/null | python3 -c "import json,sys; print(json.load(sys.stdin)['issued'])" 2>/dev/null
    $K -n $SYS delete pod "$p" --wait=false >/dev/null 2>&1
}
# The FailedMount event text for a pod (kubelet's, carrying our message).
mount_events() { $K -n $NS get events --field-selector involvedObject.name="$1" -o jsonpath='{range .items[*]}{.reason}: {.message}{"\n"}{end}' 2>/dev/null; }

# ── setup / teardown ─────────────────────────────────────────────────
if [ "${1:-}" = "setup" ]; then
    set -e
    $K apply -f rig.yaml
    $K apply -f "$REPO/flint-passthrough-chart/crds/flintpassthroughmounts.yaml"
    $K apply -f "$REPO/flint-lean-chart/crds/flintleanworkspaces.yaml" 2>/dev/null || true
    echo "waiting for MinIO + seed…"
    $K -n $SYS rollout status deploy/minio --timeout=180s
    $K -n $SYS wait --for=condition=complete job/seed-bucket --timeout=180s
    $K -n $SYS wait --for=condition=ready pod/mc-s3 --timeout=120s
    echo "seeded:"; mcx mc ls --recursive m/s3bucket/
    helm --kube-context "$CTX" upgrade --install flint-s3-csi "$REPO/flint-s3-csi-chart" -n $SYS \
        --set node.image.tag="$TAG" --set workers.passthroughImage.tag="$TAG" --set workers.leanImage.tag="$TAG" \
        --set node.image.pullPolicy=IfNotPresent \
        --set broker.backend=static --set broker.static.secretRef=s3-broker-static \
        --set node.credsLifetimeSecs="$CREDS_LIFETIME" --set broker.replicas=1 \
        --set node.logLevel=debug --set broker.logLevel=debug
    $K -n $SYS rollout status ds/flint-s3-csi-node --timeout=180s
    $K -n $SYS rollout status deploy/flint-s3-broker --timeout=180s
    $K apply -f tenants.yaml
    $K apply -f refusals.yaml
    echo "setup done"
    exit 0
fi
if [ "${1:-}" = "teardown" ]; then
    $K delete -f refusals.yaml --ignore-not-found --wait=false
    $K delete -f tenants.yaml --ignore-not-found --wait=true --timeout=180s
    helm --kube-context "$CTX" uninstall flint-s3-csi -n $SYS || true
    $K delete -f rig.yaml --ignore-not-found --wait=false
    exit 0
fi

echo "s3.csi.chert.us e2e — context $CTX, node $NODE"
# The refusal fixtures are re-applied here, not only in setup: a leg
# that finds its pod absent reports an empty event, never a verdict.
$K apply -f refusals.yaml >/dev/null

# ── S1 registration, and fail-closed ─────────────────────────────────
leg S1 "the driver is registered, the plugin is Ready, and there is NO webhook"
$K get csidriver s3.csi.chert.us >/dev/null 2>&1 && ok "CSIDriver s3.csi.chert.us exists" || bad "CSIDriver s3.csi.chert.us missing"
csinode_lists() { $K get csinode "$NODE" -o json | python3 -c "import json,sys; d=json.load(sys.stdin); sys.exit(0 if any(x['name']=='s3.csi.chert.us' for x in (d['spec'].get('drivers') or [])) else 1)"; }
i=0; while [ $i -lt 30 ] && ! csinode_lists; do sleep 2; i=$((i + 2)); done
if csinode_lists; then
    ok "CSINode $NODE lists s3.csi.chert.us (the registrar did its job, ${i}s after the plugin came up)"
else
    bad "CSINode $NODE does not list s3.csi.chert.us"
fi
ready=$($K -n $SYS get ds flint-s3-csi-node -o jsonpath='{.status.numberReady}')
[ "${ready:-0}" -ge 1 ] && ok "DaemonSet numberReady=$ready" || bad "DaemonSet numberReady=${ready:-0}"
mwc=$($K get mutatingwebhookconfigurations -o name | grep -c flint || true)
[ "$mwc" = "0" ] && ok "zero flint MutatingWebhookConfigurations" || bad "$mwc flint MutatingWebhookConfigurations present"
# Control: with the plugin gone, a NEW pod stays ContainerCreating (fail-closed).
$K -n $SYS patch ds flint-s3-csi-node -p '{"spec":{"template":{"spec":{"nodeSelector":{"chert.us/absent":"true"}}}}}' >/dev/null
sleep 8
$K -n $NS delete pod reader --ignore-not-found --wait=true --timeout=120s >/dev/null 2>&1
$K apply -f tenants.yaml >/dev/null
if wait_phase reader Running 40; then
    bad "CONTROL: reader started with the plugin absent — fail-closed does not hold"
else
    ev=$(mount_events reader)
    echo "$ev" | grep -q 'FailedMount\|kubernetes.io/csi\|not found\|driver name s3.csi.chert.us not found' && ok "CONTROL: without the plugin a new pod stays ContainerCreating with a mount event" || { note "events: $ev"; ok "CONTROL: without the plugin the pod did not start (no explicit event yet)"; }
fi
$K -n $SYS patch ds flint-s3-csi-node --type=json -p '[{"op":"remove","path":"/spec/template/spec/nodeSelector/chert.us~1absent"}]' >/dev/null
$K -n $SYS rollout status ds/flint-s3-csi-node --timeout=180s >/dev/null
wait_phase reader Running 180 && ok "the same pod mounts once the plugin is back" || bad "reader never came back after the plugin returned: $(mount_events reader | tail -2)"

# ── S2 the headline: restricted namespace, content from uid 1001 ─────
leg S2 "a RESTRICTED-namespace pod reads bucket bytes it did not write, as uid 1001"
psa=$($K get ns $NS -o jsonpath='{.metadata.labels.pod-security\.kubernetes\.io/enforce}')
[ "$psa" = "restricted" ] && ok "namespace enforces PodSecurity $psa" || bad "namespace enforces '$psa', not restricted — the leg would prove nothing"
if require_pod reader; then
    who=$(inpod reader id -u)
    [ "$who" = "1001" ] && ok "reader is uid 1001" || bad "reader is uid '$who'"
    got=$(inpod reader cat /mnt/s3/shard-05.txt)
    [ "$got" = "seeded-object-05" ] && ok "shard-05.txt = seeded-object-05" || bad "shard-05.txt = '$got'"
    n=$(inpod reader ls /mnt/s3 | wc -l | tr -d ' ')
    [ "$n" = "11" ] && ok "11 entries under the prefix" || bad "$n entries under the prefix (expected 11)"
    deep=$(inpod reader cat /mnt/s3/sub/deep.txt)
    [ "$deep" = "deep-seeded" ] && ok "sub/deep.txt = deep-seeded" || bad "sub/deep.txt = '$deep'"
    fst=$(inpod reader "grep ' /mnt/s3 ' /proc/mounts | awk '{print \$3}'")
    case "$fst" in fuse*) ok "/mnt/s3 is $fst inside the tenant container" ;; *) bad "/mnt/s3 fstype is '$fst'" ;; esac
    inpod reader "ls /mnt/s3/elsewhere /mnt/s3/private" >/dev/null 2>&1 && bad "other prefixes are visible under the mount" || ok "elsewhere/ and private/ are invisible (keyPrefix scopes the mount)"
    inpod reader "test -f /mnt/s3/only.txt" && bad "elsewhere's file is visible" || ok "elsewhere's file is not"
fi
# The falsifier: same bucket, other prefix ⇒ one file, other bytes.
if require_pod reader-elsewhere; then
    n=$(inpod reader-elsewhere ls /mnt/s3 | wc -l | tr -d ' ')
    got=$(inpod reader-elsewhere cat /mnt/s3/only.txt)
    [ "$n" = "1" ] && [ "$got" = "elsewhere-only" ] && ok "FALSIFIER: the other prefix shows exactly one file with its own bytes" || bad "FALSIFIER: other prefix shows $n entries, only.txt='$got'"
fi
# The inverted control: the OLD label, same restricted namespace — no
# webhook here, so nothing is injected and nothing is mounted.
if require_pod labelled-only; then
    ic=$($K -n $NS get pod labelled-only -o jsonpath='{.spec.initContainers[*].name}')
    [ -z "$ic" ] && ok "CONTROL: the labelled pod carries no injected sidecar (no webhook in this delivery)" || bad "CONTROL: labelled pod has initContainers '$ic'"
    inpod labelled-only "test -d /mnt/s3" && bad "CONTROL: labelled pod has /mnt/s3" || ok "CONTROL: labelled pod has no mount"
fi

# ── S3 nothing in the tenant pod ─────────────────────────────────────
leg S3 "the tenant pod holds no credential, no token, no injected anything — and the worker does"
if require_pod reader; then
    aws=$(inpod reader "env | grep -c '^AWS_'"); [ "${aws:-1}" = "0" ] && ok "zero AWS_* in the agent env" || bad "$aws AWS_* vars in the agent env"
    fl=$(inpod reader "env | grep -c '^FLINT_'"); [ "${fl:-1}" = "0" ] && ok "zero FLINT_* in the agent env" || bad "$fl FLINT_* vars in the agent env"
    inpod reader "ls /comm /var/run/secrets/flint-s3 2>/dev/null" | grep -q . && bad "token/comm files visible in the tenant" || ok "no token or comm files in the tenant container"
    # Structural: the pod SPEC (as stored) declares one csi volume, no
    # secrets, no envFrom, no initContainers.
    $K -n $NS get pod reader -o json | python3 -c "
import json,sys
p=json.load(sys.stdin)['spec']
vols=p.get('volumes',[]); csi=[v for v in vols if 'csi' in v]
secrets=[v for v in vols if 'secret' in v]
envfrom=[c for c in p['containers'] if c.get('envFrom')]
init=p.get('initContainers',[])
priv=[c for c in p['containers'] if (c.get('securityContext') or {}).get('privileged')]
bad=[]
if len(csi)!=1: bad.append('csi volumes=%d'%len(csi))
if secrets: bad.append('secret volumes')
if envfrom: bad.append('envFrom')
if init: bad.append('initContainers')
if priv: bad.append('privileged')
print('BAD '+', '.join(bad) if bad else 'OK')" | grep -q '^OK' && ok "pod spec: one csi volume, no secret volumes, no envFrom, no initContainers, nothing privileged" || bad "pod spec carries something the CSI delivery must not need"
    w=$(worker_of reader)
    if [ -n "$w" ]; then
        ok "worker pod $w exists in $WNS for reader"
        wsc=$($K -n $WNS get pod "$w" -o jsonpath='{.spec.containers[0].securityContext.privileged} {.spec.securityContext.runAsUser} {.spec.containers[0].securityContext.capabilities.drop[0]}')
        [ "$wsc" = "false 1001 ALL" ] && ok "worker is unprivileged, uid 1001, drop ALL" || bad "worker securityContext: '$wsc'"
        hp=$($K -n $WNS get pod "$w" -o jsonpath='{.spec.volumes[*].hostPath.path}')
        [ -z "$hp" ] && ok "worker has no hostPath" || bad "worker hostPath: $hp"
        # ANTI-VACUITY: the credential must be SOMEWHERE — in the worker's comm dir, owned by the worker.
        files=$($K -n $WNS exec "$w" -- ls /comm 2>/dev/null | tr '\n' ' ')
        echo "$files" | grep -q 'creds.json' && echo "$files" | grep -q 'auth.token' && ok "worker /comm holds creds.json + auth.token ($files)" || bad "worker /comm holds '$files' — where are the keys?"
        owner=$($K -n $WNS exec "$w" -- stat -c '%u %a' /comm/creds.json 2>/dev/null)
        [ "$owner" = "1001 600" ] && ok "creds.json is 0600 owned by the worker uid" || bad "creds.json owner/mode '$owner'"
        $K -n $WNS get pod "$w" -o json | grep -q 'AWS_SECRET' && bad "a secret-shaped env var is in the worker POD SPEC" || ok "no secret in the worker pod spec (it travels over the socket and the comm dir)"
    else
        bad "no worker pod found for reader"
    fi
fi

# ── S4 a missing CR ──────────────────────────────────────────────────
leg S4 "a pod naming a CR that does not exist never starts, and the event names it"
if wait_phase refused-missing Running 20; then bad "refused-missing is Running"; else
    ev=$(mount_events refused-missing)
    echo "$ev" | grep -q 'no-such-mount' && echo "$ev" | grep -q "$NS" && ok "event names the CR and the namespace" || bad "event does not name the CR: $ev"
fi
require_pod reader >/dev/null && ok "CONTROL: the pod naming a real CR is Running" || bad "CONTROL failed"

# ── S5 the pod cannot choose policy ──────────────────────────────────
leg S5 "volumeAttributes cannot choose the bucket; readOnly is honoured; RW writes land and unlink removes"
if wait_phase refused-badattr Running 20; then bad "refused-badattr is Running"; else
    ev=$(mount_events refused-badattr)
    echo "$ev" | grep -q 'bucket' && ok "event names the refused attribute 'bucket'" || bad "event: $ev"
fi
if require_pod reader-ro; then
    got=$(inpod reader-ro cat /mnt/ro/shard-03.txt)
    [ "$got" = "seeded-object-03" ] && ok "read-only mount reads" || bad "read-only read: '$got'"
    inpod reader-ro "echo x > /mnt/ro/newfile.txt" && bad "read-only mount accepted a write" || ok "read-only mount refuses a write"
    mcx mc stat m/s3bucket/datasets/imagenet/newfile.txt >/dev/null 2>&1 && bad "newfile.txt reached the bucket from the RO mount" || ok "nothing landed in the bucket from the RO mount"
fi
if require_pod reader; then
    inpod reader "echo written-by-reader > /mnt/s3/from-reader.txt" && ok "RW write accepted" || bad "RW write refused"
    sleep 2
    got=$(mcx mc cat m/s3bucket/datasets/imagenet/from-reader.txt)
    [ "$got" = "written-by-reader" ] && ok "the object exists in the bucket with the written bytes" || bad "bucket object: '$got'"
    if [ "$got" = "written-by-reader" ]; then
        inpod reader "rm /mnt/s3/from-reader.txt" && ok "unlink accepted" || bad "unlink refused"
        sleep 2
        mcx mc stat m/s3bucket/datasets/imagenet/from-reader.txt >/dev/null 2>&1 && bad "object still in the bucket after unlink" || ok "unlink removed the object"
    fi
fi

# ── S5c the interim static arm ───────────────────────────────────────
leg S5c "the static arm: the pod names a Secret, kubelet fetches it, the node SA never could"
if require_pod reader-static; then
    got=$(inpod reader-static cat /mnt/s3/shard-07.txt)
    [ "$got" = "seeded-object-07" ] && ok "static-arm mount reads content" || bad "static-arm read: '$got'"
    w=$(worker_of reader-static)
    files=$($K -n $WNS exec "$w" -- ls /comm 2>/dev/null | tr '\n' ' ')
    echo "$files" | grep -q creds.json && bad "static arm should not use the door (creds.json present)" || ok "static arm: no creds.json (keys went over the launch socket into the child env)"
fi

# ── S6 consumers ─────────────────────────────────────────────────────
leg S6 "an SA not in consumers is refused by name; an absent consumers list denies"
if wait_phase refused-bob Running 20; then bad "refused-bob is Running"; else
    ev=$(mount_events refused-bob)
    echo "$ev" | grep -q 'bob' && echo "$ev" | grep -q 'spec.consumers.serviceAccounts' && ok "event names bob and spec.consumers.serviceAccounts" || bad "event: $ev"
fi
$K -n $NS get fpm datasets -o jsonpath='{.metadata.name}' >/dev/null 2>&1 && ok "the CR exists (the refusal is about the SA, not the CR)" || bad "CR datasets missing"
if wait_phase refused-noconsumers Running 20; then bad "refused-noconsumers is Running"; else
    ev=$(mount_events refused-noconsumers)
    echo "$ev" | grep -q 'no spec.consumers.serviceAccounts' && ok "absent consumers denies and says so" || bad "event: $ev"
fi
require_pod reader >/dev/null && ok "CONTROL: the identical pod under SA trainer is Running" || bad "CONTROL failed"

# ── S7 a pod cannot mint for itself ──────────────────────────────────
leg S7 "a pod presenting its OWN projected s3.csi.chert.us token to the broker is refused (no registration)"
cat <<EOF | $K apply -f - >/dev/null
apiVersion: v1
kind: Pod
metadata: { name: self-minter, namespace: $NS }
spec:
  serviceAccountName: trainer
  securityContext: { runAsNonRoot: true, runAsUser: 1001, seccompProfile: { type: RuntimeDefault } }
  volumes:
    - name: tok
      projected: { sources: [ { serviceAccountToken: { audience: s3.csi.chert.us, expirationSeconds: 600, path: token } } ] }
  containers:
    - name: agent
      image: busybox:1.36
      command: ["/bin/sh", "-c", "trap 'exit 0' TERM INT; sleep 86400 & wait"]
      securityContext: { allowPrivilegeEscalation: false, capabilities: { drop: [ALL] } }
      volumeMounts: [{ name: tok, mountPath: /tok, readOnly: true }]
EOF
if wait_phase self-minter Running 60; then
    resp=$(inpod_out self-minter "T=\$(cat /tok/token); wget -q -O - --post-data \"Action=AssumeRoleWithWebIdentity&Version=2011-06-15&RoleArn=arn:flint:iam::passthrough:role/datasets&RoleSessionName=forged&WebIdentityToken=\$T\" http://flint-s3-broker.$SYS.svc/ 2>&1 || true")
    blog=$($K -n $SYS logs deploy/flint-s3-broker --since=60s 2>/dev/null)
    echo "$resp" | grep -q '403' && echo "$blog" | grep -q 'AccessDenied.*forged\|forged.*AccessDenied\|AccessDenied.*registration' && ok "broker refused the self-minted token with 403 AccessDenied: no live publish registration" || bad "broker answered: $(echo "$resp" | head -c 200) / log: $(echo "$blog" | grep -i refused | tail -1 | cut -c1-200)"
    # And the same token with a bogus audience claim cannot exist: the token IS aud=s3.csi.chert.us, so the refusal above is the registration check, not audience.
    resp=$(inpod_out self-minter "wget -q -O - --post-data 'Action=AssumeRoleWithWebIdentity&Version=2011-06-15&RoleArn=arn:flint:iam::passthrough:role/datasets&RoleSessionName=x&WebIdentityToken=not.a.jwt' http://flint-s3-broker.$SYS.svc/ 2>&1 || true")
    blog=$($K -n $SYS logs deploy/flint-s3-broker --since=60s 2>/dev/null)
    echo "$resp" | grep -q '400' && echo "$blog" | grep -q 'InvalidIdentityToken' && ok "a garbage token is refused with 400 InvalidIdentityToken at TokenReview" || bad "garbage token answer: $(echo "$resp" | head -c 200) / log: $(echo "$blog" | grep -i invalid | tail -1 | cut -c1-200)"
else
    bad "self-minter pod did not start"
fi
$K -n $NS delete pod self-minter --wait=false >/dev/null 2>&1

# ── S8 rotation ──────────────────────────────────────────────────────
leg S8 "rotation: short-lived keys are refreshed by republish while a reader keeps reading"
if require_pod reader; then
    w=$(worker_of reader)
    exp0=$($K -n $WNS exec "$w" -- sh -c 'cat /comm/creds.json' 2>/dev/null | python3 -c "import json,sys; print(json.load(sys.stdin)['Expiration'])" 2>/dev/null)
    issued0=$(broker_issued)
    note "creds expire $exp0; broker issued so far: ${issued0:-?}; lifetime $CREDS_LIFETIME s — reading for 150 s"
    errs=$(inpod reader "e=0; for i in \$(seq 1 30); do [ \"\$(cat /mnt/s3/shard-01.txt)\" = seeded-object-01 ] || e=\$((e+1)); sleep 5; done; echo \$e")
    [ "$errs" = "0" ] && ok "zero read errors across 150 s of reads" || bad "$errs read errors across the rotation window"
    exp1=$($K -n $WNS exec "$w" -- sh -c 'cat /comm/creds.json' 2>/dev/null | python3 -c "import json,sys; print(json.load(sys.stdin)['Expiration'])" 2>/dev/null)
    [ -n "$exp1" ] && [ "$exp1" != "$exp0" ] && ok "creds.json Expiration moved ($exp0 → $exp1): republish refreshed the key" || bad "creds.json Expiration did not move ($exp0 → $exp1)"
    issued1=$(broker_issued)
    [ "${issued1:-0}" -gt "${issued0:-0}" ] 2>/dev/null && ok "broker issued count grew (${issued0} → ${issued1})" || bad "broker issued count did not grow (${issued0} → ${issued1})"
    # CONTROL: broker gone ⇒ refresh fails ⇒ an event on the TENANT pod names it.
    $K -n $SYS scale deploy/flint-s3-broker --replicas=0 >/dev/null
    i=0; seen=""
    while [ $i -lt 240 ]; do
        seen=$(mount_events reader | grep -c CredentialRefreshFailed || true)
        [ "${seen:-0}" -ge 1 ] && break
        sleep 10; i=$((i + 10))
    done
    [ "${seen:-0}" -ge 1 ] && ok "CONTROL: with the broker down, CredentialRefreshFailed lands on the tenant pod within ${i}s" || bad "CONTROL: no CredentialRefreshFailed event in 240 s with the broker down"
    $K -n $SYS scale deploy/flint-s3-broker --replicas=1 >/dev/null
    $K -n $SYS rollout status deploy/flint-s3-broker --timeout=120s >/dev/null
fi

# ── S9 plugin restart survival, and the worker's death is the mount's ─
leg S9 "a DaemonSet roll mid-read leaves the mount serving; a dead WORKER strands its tenant and the pod is told"
if require_pod reader; then
    w=$(worker_of reader)
    wuid=$($K -n $WNS get pod "$w" -o jsonpath='{.metadata.uid}')
    sum0=$(inpod reader "cat /mnt/s3/shard-0*.txt | md5sum | cut -c1-32")
    $K -n $SYS rollout restart ds/flint-s3-csi-node >/dev/null
    sleep 3
    sum1=$(inpod reader "cat /mnt/s3/shard-0*.txt | md5sum | cut -c1-32")
    $K -n $SYS rollout status ds/flint-s3-csi-node --timeout=180s >/dev/null
    sum2=$(inpod reader "cat /mnt/s3/shard-0*.txt | md5sum | cut -c1-32")
    [ -n "$sum0" ] && [ "$sum0" = "$sum1" ] && [ "$sum1" = "$sum2" ] && ok "reads before/during/after the plugin roll agree ($sum0)" || bad "reads diverged across the roll: $sum0 / $sum1 / $sum2"
    wuid2=$($K -n $WNS get pod "$w" -o jsonpath='{.metadata.uid}')
    [ "$wuid" = "$wuid2" ] && ok "the worker pod is the same object across the roll (uid unchanged)" || bad "worker was replaced across the roll"
    fm=$(mount_events reader | grep -c FailedMount || true)
    note "FailedMount events on reader so far: $fm (S1's control contributes)"
    # CONTROL: kill the worker ⇒ ENOTCONN for the tenant, and an event.
    # Killed at the RUNTIME (crictl stopp on the sandbox): the admission
    # policy refuses an API delete from anyone but the node plugin, and a
    # graceful delete would be a different death than the one §6.1 fears.
    # SIGKILL the worker CONTAINER (crictl stop -t 0). crictl's own CRI
    # client deadline is 2 s and the runtime takes longer to confirm the
    # stop, so its exit status is not the verdict — the tenant's read is.
    onnode "crictl --timeout 60s stop -t 0 \$(crictl ps -q --pod \$(crictl pods --name $w -q))" >/dev/null 2>&1 || note "crictl reported an error stopping worker $w (its CRI deadline); the read below decides"
    sleep 8
    if inpod reader "cat /mnt/s3/shard-01.txt" >/dev/null 2>&1; then
        bad "CONTROL: reads still succeed after the worker died — the leg above would be vacuous"
    else
        ok "CONTROL: with the worker dead the tenant's read fails (ENOTCONN class)"
    fi
    i=0; seen=""
    while [ $i -lt 200 ]; do
        seen=$(mount_events reader | grep -c MounterDead || true)
        [ "${seen:-0}" -ge 1 ] && break
        sleep 10; i=$((i + 10))
    done
    [ "${seen:-0}" -ge 1 ] && ok "a MounterDead Warning landed on the tenant pod within ${i}s (the plugin's own event; kubelet emits none)" || bad "no MounterDead event within 200 s"
    # The pod must be recreated (FUSE semantics). Do it, and prove it comes back.
    $K -n $NS delete pod reader --wait=true --timeout=180s >/dev/null 2>&1
    $K apply -f tenants.yaml >/dev/null
    wait_phase reader Running 180 && [ "$(inpod reader cat /mnt/s3/shard-05.txt)" = "seeded-object-05" ] && ok "a recreated pod mounts and reads again" || bad "recreated reader did not come back reading"
fi

# ── S10 revocation ───────────────────────────────────────────────────
leg S10 "removing the SA from consumers stops the mount within one key lifetime; a sibling keeps reading"
if require_pod reader && require_pod reader-ro; then
    $K -n $NS patch fpm datasets --type=merge -p '{"spec":{"consumers":{"serviceAccounts":["nobody"]}}}' >/dev/null
    note "consumers on datasets now [nobody]; waiting up to lifetime+republish+slack ($((CREDS_LIFETIME + 240)) s) for reads to fail"
    i=0; failed=""
    while [ $i -lt $((CREDS_LIFETIME + 240)) ]; do
        if ! inpod reader "cat /mnt/s3/shard-02.txt" >/dev/null 2>&1; then failed=yes; break; fi
        sleep 10; i=$((i + 10))
    done
    [ "$failed" = "yes" ] && ok "reads on the revoked project fail after ${i}s" || bad "reads still succeed $((CREDS_LIFETIME + 240)) s after revocation"
    got=$(inpod reader-ro cat /mnt/ro/shard-04.txt)
    [ "$got" = "seeded-object-04" ] && ok "the untouched sibling (datasets-ro) keeps reading" || bad "sibling read: '$got'"
    mount_events reader | grep -q CredentialRefreshFailed && ok "the tenant pod carries a CredentialRefreshFailed event naming the refusal" || bad "no CredentialRefreshFailed event on the revoked pod"
    $K -n $NS patch fpm datasets --type=merge -p '{"spec":{"consumers":{"serviceAccounts":["trainer"]}}}' >/dev/null
fi

# ── S15 two projects, one node ───────────────────────────────────────
leg S15 "two projects on one node: two workers, two keys, no cross-visibility"
if require_pod reader-ro && require_pod reader-elsewhere; then
    wa=$(worker_of reader-ro); wb=$(worker_of reader-elsewhere)
    [ -n "$wa" ] && [ -n "$wb" ] && [ "$wa" != "$wb" ] && ok "distinct workers: $wa, $wb" || bad "workers: '$wa' '$wb'"
    na=$($K -n $WNS exec "$wa" -- cat /comm/auth.token 2>/dev/null); nb=$($K -n $WNS exec "$wb" -- cat /comm/auth.token 2>/dev/null)
    [ -n "$na" ] && [ "$na" != "$nb" ] && ok "distinct per-volume nonces" || bad "nonces: '$na' '$nb'"
    inpod reader-elsewhere "test -f /mnt/s3/shard-01.txt" && bad "elsewhere sees datasets" || ok "elsewhere does not see datasets"
    inpod reader-ro "test -f /mnt/ro/only.txt" && bad "datasets-ro sees elsewhere" || ok "datasets-ro does not see elsewhere"
fi

# ── S19 RBAC + admission policy ──────────────────────────────────────
leg S19 "the node SA can read no Secret anywhere and create pods only in $WNS; the VAP pins workers to the caller's node"
SA="system:serviceaccount:$SYS:flint-s3-csi-node"
[ "$($K auth can-i get secrets -n $NS --as="$SA")" = "no" ] && ok "node SA cannot get secrets in $NS" || bad "node SA CAN get secrets in $NS"
[ "$($K auth can-i get secrets -n $SYS --as="$SA")" = "no" ] && ok "node SA cannot get secrets in $SYS" || bad "node SA CAN get secrets in $SYS"
[ "$($K auth can-i create pods -n $NS --as="$SA")" = "no" ] && ok "node SA cannot create pods in $NS" || bad "node SA CAN create pods in $NS"
[ "$($K auth can-i create pods -n $WNS --as="$SA")" = "yes" ] && ok "node SA can create pods in $WNS" || bad "node SA cannot create pods in $WNS"
# Impersonation carries no node-name extra, so the VAP must refuse.
out=$(cat <<EOF | $K --as="$SA" apply -f - 2>&1 || true
apiVersion: v1
kind: Pod
metadata: { name: forged-worker, namespace: $WNS, labels: { app.kubernetes.io/managed-by: flint-s3-csi-node } }
spec:
  nodeName: $NODE
  automountServiceAccountToken: false
  containers: [{ name: worker, image: dilipdalton/flint-s3-worker:$TAG, command: ["/bin/sleep", "60"] }]
EOF
)
echo "$out" | grep -qi 'denied\|ValidatingAdmissionPolicy\|node-name' && ok "VAP refused a worker created without the caller's node identity: $(echo "$out" | head -c 120)" || bad "VAP did not refuse the forged worker: $(echo "$out" | head -c 200)"
$K -n $WNS delete pod forged-worker --ignore-not-found --wait=false >/dev/null 2>&1

# ── S-unpublish teardown hygiene ─────────────────────────────────────
# ── lean (design §3.5, §5; S11 + S13) ────────────────────────────────
# Store helpers, the lean drill's (run-agent.sh): jq, not grep — the
# manifest is nested JSON.
lobj()   { mcx mc cat "m/s3bucket/$1" 2>/dev/null; }
lcount() { mcx mc ls --recursive "m/s3bucket/$1" 2>/dev/null | grep -c . ; }
lmseq()  { local m; m=$(lobj "$1/.flint/lean/manifest"); [ -z "$m" ] && { echo 0; return; }; printf '%s' "$m" | jq -r '.seq // 0'; }
lmhas()  { local m; m=$(lobj "$1/.flint/lean/manifest"); [ -z "$m" ] && return 1; printf '%s' "$m" | jq -e --arg p "$2" '.entries | has($p)' >/dev/null; }

leg S11 "lean: the checkout gate holds for the app AND its init container; a cold pod finds the seeded project; the syncer lives in the worker, not the pod"
$K -n $NS delete pod lean-agent lean-seeder lean-refused --ignore-not-found --wait=true --timeout=180s >/dev/null 2>&1
mcx mc rm --recursive --force m/s3bucket/tenants/proj/ >/dev/null 2>&1
$K apply -f lean-tenants.yaml >/dev/null
if wait_phase lean-seeder Running 300; then
    ok "lean-seeder is Running (claim + checkout of a fresh prefix, no operator, no webhook)"
    i=0; while [ $i -lt 120 ] && ! $K -n $NS logs lean-seeder 2>/dev/null | grep -q 'SEED PUBLISHED'; do sleep 2; i=$((i + 2)); done
    $K -n $NS logs lean-seeder 2>/dev/null | grep -q 'SEED PUBLISHED' && ok "the seeder's declared publish was acked in ${i}s" || bad "seeder log: $($K -n $NS logs lean-seeder 2>/dev/null | tail -2)"
    n=$(lcount tenants/proj/files/src/)
    [ "$n" = "200" ] && ok "200 objects under tenants/proj/files/src/" || bad "the bucket holds '$n' objects under files/src/, not 200"
    lmhas tenants/proj src/f0042.txt && ok "the manifest cites src/f0042.txt at seq $(lmseq tenants/proj)" || bad "the manifest does not cite src/f0042.txt"
    # The tenant pod: one csi volume, nothing else; the worker: the syncer, uid 1001, ONE hostPath under the plugin dir.
    [ "$(inpod lean-seeder "env | grep -c '^FLINT_SYNC_\|^AWS_'")" = "0" ] && ok "zero FLINT_SYNC_*/AWS_* in the seeder's env" || bad "the seeder's env carries syncer or AWS variables"
    w=$(worker_of lean-seeder)
    if [ -n "$w" ]; then
        wsc=$($K -n $WNS get pod "$w" -o jsonpath='{.spec.securityContext.runAsUser} {.spec.containers[0].securityContext.capabilities.drop[0]} {.spec.containers[0].securityContext.privileged}')
        [ "$wsc" = "1001 ALL false" ] && ok "syncer worker $w is uid 1001, drop ALL, unprivileged" || bad "syncer worker securityContext: '$wsc'"
        hp=$($K -n $WNS get pod "$w" -o jsonpath='{.spec.volumes[*].hostPath.path}')
        case "$hp" in /var/lib/kubelet/plugins/s3.csi.chert.us/volumes/*/tree) ok "its only hostPath is the plugin-owned tree ($hp)";; *) bad "syncer hostPath: '$hp'";; esac
        [ "$($K -n $WNS exec "$w" -- sh -c 'ps | grep -c "[f]lint-sync run"' 2>/dev/null)" -ge 1 ] && ok "flint-sync run is alive in the worker" || bad "no flint-sync run process in the worker"
    else
        bad "no Running worker for lean-seeder"
    fi
    # The cold agent: created only after the seeder pod is GONE.
    $K -n $NS delete pod lean-seeder --wait=true --timeout=180s >/dev/null 2>&1
    $K -n $NS get pod lean-seeder >/dev/null 2>&1 && bad "the seeder pod still exists" || ok "the seeder pod is gone; only bucket objects remain"
    $K apply -f lean-agent.yaml >/dev/null
    if wait_phase lean-agent Running 300; then
        [ "$(inpod lean-agent cat /scratch/init-gate)" = "GATE-OK" ] && ok "the INIT container ran only after checkout-complete existed" || bad "the init container saw no checkout-complete marker"
        [ "$(inpod lean-agent cat /scratch/init-seen | tr -d ' ')" = "200" ] && ok "the init container saw all 200 files" || bad "the init container saw $(inpod lean-agent cat /scratch/init-seen) files"
        [ "$(inpod lean-agent cat /tmp/gate)" = "GATE-OK" ] && ok "the app's first instruction ran after checkout-complete" || bad "the app ran before checkout-complete"
        [ "$(inpod lean-agent cat /tmp/seen-count | tr -d ' ')" = "200" ] && ok "the app's first instruction saw 200 files" || bad "the app saw $(inpod lean-agent cat /tmp/seen-count) files"
        [ "$(inpod lean-agent cat /tmp/seen-sample)" = "unit 0042 of the seeded project" ] && ok "f0042.txt carries the seeded bytes" || bad "f0042.txt read back as '$(inpod lean-agent cat /tmp/seen-sample)'"
        [ "$(inpod lean-agent cat /tmp/aws-count)$(inpod lean-agent cat /tmp/flint-count)" = "00" ] && ok "zero AWS_*/FLINT_SYNC_* in the agent's env" || bad "the agent's env carries credentials or syncer config"
    else
        bad "lean-agent never became Running: $(mount_events lean-agent | tail -2)"
    fi
else
    bad "lean-seeder never became Running: $(mount_events lean-seeder | tail -3)"
fi
# CONTROL: the lean CRD's consumers gate — absent list ⇒ deny, by name.
if wait_phase lean-refused Running 30; then bad "CONTROL: lean-refused is Running against a CR with no consumers"; else
    ev=$(mount_events lean-refused)
    echo "$ev" | grep -q 'spec.consumers.serviceAccounts' && echo "$ev" | grep -q 'trainer' && ok "CONTROL: a lean CR without consumers denies, naming the SA and the field" || bad "CONTROL: lean-refused event: $(echo "$ev" | tail -1 | cut -c1-200)"
fi

leg S13 "lean drain: a file written after the last publish is in the bucket, cited, once the pod is deleted — and the worker follows the pod"
if require_pod lean-agent; then
    inpod lean-agent "printf 'written after the seed publish\n' > /workspace/src/late.txt" || bad "could not write late.txt"
    sleep 5
    lmhas tenants/proj src/late.txt && bad "PRECONDITION: late.txt is already cited before the delete — a tick published it, the drain leg proves nothing" || ok "PRECONDITION: late.txt is not cited before the delete (floorSecs is an hour)"
    w=$(worker_of lean-agent)
    $K -n $NS delete pod lean-agent --wait=true --timeout=180s >/dev/null 2>&1
    i=0; while [ $i -lt 120 ] && ! lmhas tenants/proj src/late.txt; do sleep 2; i=$((i + 2)); done
    lmhas tenants/proj src/late.txt && ok "the drain published late.txt (cited ${i}s after the delete returned) at seq $(lmseq tenants/proj)" || bad "late.txt is not cited 120 s after the pod was deleted"
    [ "$(lobj tenants/proj/files/src/late.txt)" = "written after the seed publish" ] && ok "late.txt carries the bytes the agent wrote" || bad "late.txt in the bucket reads '$(lobj tenants/proj/files/src/late.txt)'"
    i=0; while [ $i -lt 90 ] && [ -n "$w" ] && $K -n $WNS get pod "$w" >/dev/null 2>&1; do sleep 2; i=$((i + 2)); done
    [ -z "$w" ] || $K -n $WNS get pod "$w" >/dev/null 2>&1 && bad "syncer worker $w still exists ${i}s after its tenant was deleted" || ok "syncer worker ${w:-?} is gone"
    trees=$(onnode "ls -d /var/lib/kubelet/plugins/s3.csi.chert.us/volumes/*/tree 2>/dev/null | wc -l")
    [ "${trees:-0}" = "0" ] && ok "no lean tree remains under the plugin dir" || bad "$trees lean trees remain on the node"
fi
$K -n $NS delete pod lean-refused --ignore-not-found --wait=false >/dev/null 2>&1

leg SU "deleting the tenant pod removes its worker, its state, and its mounts on the node"
if require_pod reader-elsewhere; then
    w=$(worker_of reader-elsewhere)
    $K -n $NS delete pod reader-elsewhere --wait=true --timeout=180s >/dev/null 2>&1
    i=0; while [ $i -lt 60 ] && $K -n $WNS get pod "$w" >/dev/null 2>&1; do sleep 2; i=$((i + 2)); done
    $K -n $WNS get pod "$w" >/dev/null 2>&1 && bad "worker $w still exists ${i}s after the tenant was deleted" || ok "worker $w is gone"
    vols=$(onnode "ls /var/lib/kubelet/plugins/s3.csi.chert.us/volumes 2>/dev/null | wc -l")
    live=$($K -n $NS get pods -o name | grep -c 'reader' || true)
    note "state dirs on the node: ${vols:-?}; live readers: $live"
    stale=$(onnode "grep -c 'plugins/s3.csi.chert.us/volumes' /proc/mounts")
    [ "${stale:-0}" -le "$((live * 1))" ] && ok "plugin-dir mounts on the node ($stale) do not exceed live readers ($live)" || bad "stale plugin-dir mounts on the node: $stale for $live readers"
    $K apply -f tenants.yaml >/dev/null
fi

# ── roster ────────────────────────────────────────────────────────────
echo
for want in S1 S2 S3 S4 S5 S5c S6 S7 S8 S9 S10 S11 S13 S15 S19 SU; do
    echo " $RAN_LEGS " | grep -q " $want " || bad "leg $want never ran"
done
echo "════════════════════════════════════════"
echo "s3.csi.chert.us e2e: $PASS ok, $FAILED bad"
[ "$FAILED" = "0" ]
