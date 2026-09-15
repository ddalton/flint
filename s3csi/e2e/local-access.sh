#!/usr/bin/env bash
# Read access on the LOCAL kind rig: what the EC2 access drill
# (aws-access.sh) could not reach from a trove cluster (per-user access
# design §10.6, §10.8). THREE ARMS:
#
#   O  a real OIDC STS. flint-s3-broker's sts backend calls MinIO's
#      AssumeRoleWithWebIdentity with the pod's own kubelet token, and MinIO
#      verifies it against the cluster's service-account issuer. The EC2
#      drill needed a SigV4 stand-in (sts-shim.py) because a trove cluster's
#      issuer is not public; an STS inside the cluster can read it.
#   G  gVisor. Tenants under runsc (RuntimeClass gvisor): F7, the reader's
#      EROFS, its host bind and its gofer's mount, and the UDS door.
#   I  AccessIsolation. The lean operator's condition per identity mode, and
#      its transition when the mode changes.
#
# Legs:
#   O0  MinIO's own STS, probed with a pod's projected token: a token for
#       another audience is refused; a token for s3.csi.chert.us is exchanged,
#       and with no session policy the role writes and reads anywhere (the
#       control every narrowing below is measured against)
#   O1  the broker on sts → MinIO, through sts-tap.py (a logging pass-through),
#       reports readEnforcement sessionPolicy
#   O2  a mixed lean workspace: a writer and a reader; the broker's issued
#       lines say readWrite for one and read/sessionPolicy for the other
#   O2t the tap's own record, not the broker's: the reader's exchange carried
#       a policy of reads on the workspace prefix only, the writer's none
#   O3  the keys the reader's worker HOLDS: PUT and DELETE under its prefix
#       and GET of another prefix denied, GET under it allowed; the writer's
#       keys PUT (the same role, no policy)
#   O4  the writer's publish reaches the reader
#   O5  passthrough on a read grant: mount-s3 reads the writer's file under the
#       session policy that no longer names s3:GetObjectAttributes, and
#       refuses a write
#   G1  F7 under runsc: 1000 files, then an immediate publish; the manifest
#       cites every file at the size written; a runc reader converges to the
#       same bytes (and did not have them before the publish)
#   G1c the same from a runc writer, the control F7 names
#   G2  a runsc reader: EROFS in the tree and in .flint/, and it reads G1's bytes
#   G3  the runsc reader's host bind is ro, and so is the mount its gofer
#       serves /workspace from; the runsc writer's are rw (control)
#   G4  the UDS door: the socket is bound in each tenant's own tree; a runc
#       tenant connects, a runsc tenant is refused (§5: runsc exposes no host
#       socket without --host-uds)
#   I1  the lean operator: static, ambient → False/Cooperative; broker,
#       webIdentity, no identity → Unknown/DecidedByBroker; no consumers → no
#       condition, on a workspace the operator did reconcile
#   I2  static → broker: the condition follows, with a new lastTransitionTime
#
#   CTX=kind-flint-s3csi ./run-s3csi.sh setup     # MinIO, chart, CRDs (after build-images.sh)
#   CTX=kind-flint-s3csi ./local-access.sh setup  # runsc in the node, MinIO OIDC, operator image
#   CTX=kind-flint-s3csi ./local-access.sh        # the legs; ARMS="O G I" by default
#
# `setup` restarts containerd in the kind node once (to add the runsc
# handler) and MinIO once (its storage is ephemeral, so it re-seeds). The
# lean operator image is built from spdk-csi-driver's aarch64/x86_64 musl
# flint-lean-operator, which build-images.sh does not build: `cargo zigbuild
# --release --target <triple> --bin flint-lean-operator` first.
set -u
cd "$(dirname "$0")"
export STORE=minio NODE_EXEC=docker
REPO=$(cd ../.. && pwd)
eval "$(sed -n '/^CTX=\${CTX:-/,/^# ── setup \/ teardown/p' run-s3csi.sh | sed '$d')"
eval "$(sed -n '/^lobj()   {/,/^lmhas()  {/p' run-s3csi.sh)"
eval "$(sed -n '/^wenvv() {/,/^clear_pods() {/p' aws-access.sh)"
WORK=${WORK:-/tmp/flint-local-access}
mkdir -p "$WORK"
GVISOR_URL=https://storage.googleapis.com/gvisor/releases/release/latest
case "$(docker info --format '{{.Architecture}}' 2>/dev/null)" in
    aarch64|arm64) GARCH=aarch64; TRIPLE=aarch64-unknown-linux-musl ;;
    *)             GARCH=x86_64;  TRIPLE=x86_64-unknown-linux-musl ;;
esac
ARN_FILE=$WORK/minio-role-arn

# ── setup ─────────────────────────────────────────────────────────────
if [ "${1:-}" = setup ]; then
    set -e
    if ! docker exec "$NODE" grep -q 'runtimes.runsc' /etc/containerd/config.toml; then
        echo "installing runsc ($GARCH) into $NODE"
        ( cd "$WORK" && curl -fsSLO "$GVISOR_URL/$GARCH/gvisor.tar.bz2" && curl -fsSLO "$GVISOR_URL/$GARCH/gvisor.tar.bz2.sha512" \
            && shasum -a 512 -c gvisor.tar.bz2.sha512 && tar xjf gvisor.tar.bz2 runsc containerd-shim-runsc-v1 )
        docker exec -i "$NODE" sh -c 'cat > /usr/local/bin/runsc && chmod +x /usr/local/bin/runsc' < "$WORK/runsc"
        docker exec -i "$NODE" sh -c 'cat > /usr/local/bin/containerd-shim-runsc-v1 && chmod +x /usr/local/bin/containerd-shim-runsc-v1' < "$WORK/containerd-shim-runsc-v1"
        docker exec "$NODE" sh -c '
cat >> /etc/containerd/config.toml <<EOF

[plugins."io.containerd.grpc.v1.cri".containerd.runtimes.runsc]
  runtime_type = "io.containerd.runsc.v1"
  [plugins."io.containerd.grpc.v1.cri".containerd.runtimes.runsc.options]
    TypeUrl = "io.containerd.runsc.v1.options"
    ConfigPath = "/etc/containerd/runsc.toml"
EOF
printf "[runsc_config]\n  systemd-cgroup = \"true\"\n  platform = \"systrap\"\n" > /etc/containerd/runsc.toml
systemctl restart containerd'
        sleep 10
    fi
    printf 'apiVersion: node.k8s.io/v1\nkind: RuntimeClass\nmetadata: { name: gvisor }\nhandler: runsc\n' | $K apply -f - >/dev/null
    echo "runsc: $(docker exec "$NODE" runsc --version | head -1)"

    # MinIO's OpenID provider is the cluster's service-account issuer. The
    # discovery document and the JWKS need no credential here (rig only).
    $K create clusterrolebinding rig-oidc-discovery-anon --clusterrole=system:service-account-issuer-discovery \
        --group=system:unauthenticated --dry-run=client -o yaml | $K apply -f - >/dev/null
    $K -n $SYS patch deploy minio --type strategic -p '
spec:
  template:
    spec:
      volumes:
        - name: k8s-ca
          configMap: { name: kube-root-ca.crt, items: [{ key: ca.crt, path: k8s-ca.crt }] }
      containers:
        - name: minio
          args: ["server", "/data", "--address", ":9000", "--certs-dir", "/certs"]
          env:
            - { name: MINIO_IDENTITY_OPENID_CONFIG_URL, value: "https://kubernetes.default.svc.cluster.local/.well-known/openid-configuration" }
            - { name: MINIO_IDENTITY_OPENID_CLIENT_ID, value: "s3.csi.chert.us" }
            - { name: MINIO_IDENTITY_OPENID_ROLE_POLICY, value: "readwrite" }
          volumeMounts:
            - { name: k8s-ca, mountPath: /certs/CAs }' >/dev/null
    $K -n $SYS rollout status deploy/minio --timeout=180s >/dev/null
    arn=""; i=0
    while [ -z "$arn" ] && [ $i -lt 60 ]; do
        arn=$($K -n $SYS logs deploy/minio 2>/dev/null | grep -o 'arn:minio:iam:::role/[A-Za-z0-9_-]*' | tail -1); sleep 2; i=$((i + 2))
    done
    [ -n "$arn" ] || { echo "MinIO printed no OpenID role ARN" >&2; exit 1; }
    echo "$arn" > "$ARN_FILE"; echo "MinIO OpenID role: $arn"
    $K -n $SYS delete job seed-bucket --ignore-not-found --wait=true >/dev/null
    python3 -c "
import yaml
for d in yaml.safe_load_all(open('rig.yaml')):
    if d and d.get('kind') == 'Job': print(yaml.safe_dump(d))" | $K apply -f - >/dev/null
    $K -n $SYS wait --for=condition=complete job/seed-bucket --timeout=180s >/dev/null
    $K -n $SYS delete pod mc-s3 --ignore-not-found --wait=true >/dev/null
    python3 -c "
import yaml
for d in yaml.safe_load_all(open('rig.yaml')):
    if d and d.get('kind') == 'Pod' and d['metadata']['name'] == 'mc-s3': print(yaml.safe_dump(d))" | $K apply -f - >/dev/null
    $K -n $SYS wait --for=condition=ready pod/mc-s3 --timeout=120s >/dev/null
    echo "re-seeded: $(mcx mc ls --recursive m/$BUCKET/ | grep -c .) objects"

    cat <<'EOF' | $K apply -f - >/dev/null
apiVersion: v1
kind: Pod
metadata: { name: awscli, namespace: flint-system }
spec:
  containers:
    - name: c
      image: amazon/aws-cli:latest
      command: ["sh", "-c", "trap 'exit 0' TERM; sleep 86400 & wait"]
EOF
    $K -n $SYS wait --for=condition=ready pod/awscli --timeout=300s >/dev/null

    bin=$REPO/spdk-csi-driver/target/$TRIPLE/release/flint-lean-operator
    [ -x "$bin" ] || { echo "no $bin — cargo zigbuild --release --target $TRIPLE --bin flint-lean-operator" >&2; exit 1; }
    rm -rf "$WORK/opimg" && mkdir -p "$WORK/opimg" && cp "$bin" "$WORK/opimg/"
    printf 'FROM ubuntu:24.04\nRUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*\nCOPY flint-lean-operator /usr/local/bin/flint-lean-operator\nUSER 65532:65532\nENTRYPOINT ["/usr/local/bin/flint-lean-operator"]\n' > "$WORK/opimg/Dockerfile"
    docker build -q -t "dilipdalton/flint-lean-operator:$TAG" "$WORK/opimg" >/dev/null
    kind load docker-image --name "${CTX#kind-}" "dilipdalton/flint-lean-operator:$TAG" >/dev/null
    echo "setup done"
    exit 0
fi

# ── helpers (over run-s3csi.sh's and aws-access.sh's) ─────────────────
OUT=${OUT:-$REPO/s3csi/e2e/results/local-access-$(date +%Y-%m-%d)}
mkdir -p "$OUT"
ARMS=${ARMS:-O G I}
RUN=$(date +%H%M%S)
SKIPPED=0
# The kind node is the host: kubelet's mount table is the node container's.
host_bind() {
    local u; u=$(pod_uid "$1")
    onnode "grep '$u/volumes/kubernetes.io~csi/ws/mount ' /proc/self/mountinfo | head -1 | awk '{for (i=1;i<=NF;i++) if (\$i==\"-\") {print \$6, \$(i+2); exit}}'"
}
awsx() {
    local ak sk tok; set -- $1 "${@:2}"; ak=$1; sk=$2; tok=$3; shift 3
    if [ "$tok" = "-" ]; then
        $K -n $SYS exec awscli -- env -u AWS_SESSION_TOKEN AWS_ACCESS_KEY_ID="$ak" AWS_SECRET_ACCESS_KEY="$sk" AWS_DEFAULT_REGION="$S3_REGION" aws --endpoint-url "$S3_ENDPOINT" "$@" 2>&1
    else
        $K -n $SYS exec awscli -- env AWS_ACCESS_KEY_ID="$ak" AWS_SECRET_ACCESS_KEY="$sk" AWS_SESSION_TOKEN="$tok" AWS_DEFAULT_REGION="$S3_REGION" aws --endpoint-url "$S3_ENDPOINT" "$@" 2>&1
    fi
}
denied() { echo "$1" | grep -qE 'AccessDenied|\(403\)'; }
ROOT_KEYS="drill drillsecret -"
# A tenant: accpod's template, on a runtime (runc | gvisor) and an image.
tpod() { # name sa selkey cr ro runtime [image]
    sed -e "s#__NAME__#$1#g" -e "s#__SA__#$2#g" -e "s#__NODE__#$NODE#g" -e "s#__SELKEY__#$3#g" \
        -e "s#__CR__#$4#g" -e "s#__RO__#$5#g" -e "s|__OBO__|#|g" -e "s#image: busybox:1.36#image: ${7:-busybox:1.36}#" acc-pod.yaml.tpl \
        | awk -v rt="$6" '{print} /^spec:$/ && rt == "gvisor" {print "  runtimeClassName: gvisor"}' | $K apply -f - >/dev/null
}
# The sandbox kernel a tenant runs on: "gvisor" or "host".
kernel_of() { case "$($K -n $NS exec "$1" -c agent -- cat /proc/version 2>/dev/null)" in *gvisor*) echo gvisor ;; "") echo none ;; *) echo host ;; esac; }
# "opts fs" of the /workspace mount the runsc gofer for a pod's agent
# container serves from, read from the gofer's own mount namespace.
gofer_view() {
    docker exec -i "$NODE" bash -s "$1" <<'EOF'
want=$1
for d in /proc/[0-9]*; do
    [ "$(tr '\0' '\n' < $d/cmdline 2>/dev/null | head -1)" = runsc-gofer ] || continue
    b=$(tr '\0' '\n' < $d/cmdline | grep -o 'k8s.io/[0-9a-f]\{64\}' | head -1 | cut -d/ -f2)
    info=$(crictl inspect "$b" 2>/dev/null)
    echo "$info" | grep -q "\"io.kubernetes.pod.name\": \"$want\"" || continue
    echo "$info" | grep -q '"io.kubernetes.container.name": "agent"' || continue
    awk '$5 == "/workspace" {for (i=1;i<=NF;i++) if ($i=="-") {print $6, $(i+1); exit}}' $d/mountinfo
done
EOF
}
# Write N files of sizes 17.. N+16 under a directory, in a tenant.
tfiles() { $K -n $NS exec "$1" -c agent -- sh -c "mkdir -p /workspace/$2 && i=1; while [ \$i -le $3 ]; do head -c \$((i + 16)) /dev/urandom > /workspace/$2/f\$i.bin || exit 1; i=\$((i + 1)); done; echo ok" 2>&1 | tail -1; }
# One digest over a directory's names and bytes, as a tenant sees it.
tdigest() { $K -n $NS exec "$1" -c agent -- sh -c "cd /workspace/$2 2>/dev/null && md5sum f*.bin | sort -k2 | md5sum | cut -d' ' -f1" 2>/dev/null; }
wait_digest() { # pod dir want secs
    local i=0
    while [ $i -lt "$4" ]; do [ "$(tdigest "$1" "$2")" = "$3" ] && return 0; sleep 3; i=$((i + 3)); done
    return 1
}
# How many of dir's N files the CURRENT manifest cites at the size written.
cited_at_size() { # prefix dir n
    lmbody "$1" | python3 -c "
import json, sys
e = json.load(sys.stdin)['entries']
print(sum(1 for i in range(1, $3 + 1) if e.get('$2/f%d.bin' % i, {}).get('size') == i + 16))" 2>/dev/null
}
cond() { # workspace field  (status | reason | lastTransitionTime)
    $K -n $NS get flintleanworkspace "$1" -o jsonpath="{.status.conditions[?(@.type==\"AccessIsolation\")].$2}" 2>/dev/null
}
clear_local() { $K -n $NS delete pods -l suite=acc --ignore-not-found --wait=true --timeout=600s >/dev/null 2>&1; }

echo "flint read access on the local kind rig — $CTX, node $NODE, arms: $ARMS"
echo "evidence: $OUT"
$K get csidriver s3.csi.chert.us >/dev/null 2>&1 || { echo "no s3.csi.chert.us — run run-s3csi.sh setup first"; exit 2; }
[ -s "$ARN_FILE" ] || { echo "no MinIO role ARN — run $0 setup first"; exit 2; }
ARN=$(cat "$ARN_FILE")
sed -e "s#__B__#$BUCKET#g" -e "s#__ENDPOINT__#$S3_ENDPOINT#g" -e "s#__REGION__#$S3_REGION#g" acc-tenants.yaml.tpl | $K apply -f - >/dev/null \
    || { echo "the access tenants were refused"; exit 2; }
$K apply -f local-access-tenants.yaml >/dev/null || { echo "the local tenants were refused"; exit 2; }
clear_local
mcx sh -c "printf 'not-for-readers\n' | mc pipe m/$BUCKET/private/access-secret.txt" >/dev/null 2>&1
mcx sh -c "printf 'under-the-prefix\n' | mc pipe m/$BUCKET/access/lean/_drill/seed.txt" >/dev/null 2>&1
[ "$(awsx "$ROOT_KEYS" s3 cp "s3://$BUCKET/private/access-secret.txt" - 2>/dev/null)" = not-for-readers ] \
    && ok "PRECONDITION: the other-prefix fixture exists and the aws-cli pod reads it" \
    || bad "PRECONDITION: cannot read the other-prefix fixture — every denial below would be vacuous"

if echo " $ARMS " | grep -q " O "; then
# ══ ARM O: a real OIDC STS ════════════════════════════════════════════
leg O0 "MinIO's STS trusts the cluster issuer, and without a session policy its role writes anywhere"
$K -n $NS create sa sts-probe --dry-run=client -o yaml | $K apply -f - >/dev/null
$K -n $NS delete pod sts-probe --ignore-not-found --wait=true >/dev/null 2>&1
cat <<'EOF' | $K apply -f - >/dev/null
apiVersion: v1
kind: Pod
metadata: { name: sts-probe, namespace: s3-tenants, labels: { suite: acc } }
spec:
  serviceAccountName: sts-probe
  restartPolicy: Never
  automountServiceAccountToken: false
  securityContext: { runAsNonRoot: true, runAsUser: 1001, runAsGroup: 1001, seccompProfile: { type: RuntimeDefault } }
  volumes:
    - name: tok
      projected:
        sources:
          - serviceAccountToken: { audience: s3.csi.chert.us, expirationSeconds: 3600, path: good }
          - serviceAccountToken: { audience: some-other-audience, expirationSeconds: 3600, path: wrongaud }
  containers:
    - name: agent
      image: amazon/aws-cli:latest
      command: ["sh", "-c", "trap 'exit 0' TERM; sleep 3600 & wait"]
      env: [{ name: HOME, value: /tmp }, { name: AWS_DEFAULT_REGION, value: us-east-1 }]
      securityContext: { allowPrivilegeEscalation: false, capabilities: { drop: [ALL] } }
      volumeMounts: [{ name: tok, mountPath: /tok }]
EOF
if $K -n $NS wait --for=condition=ready pod/sts-probe --timeout=300s >/dev/null 2>&1; then
    assume() { # token
        $K -n $NS exec sts-probe -c agent -- sh -c "aws sts assume-role-with-web-identity --endpoint-url $S3_ENDPOINT --role-arn $ARN \
            --role-session-name o0 --web-identity-token \"\$(cat /tok/$1)\" --duration-seconds 900 \
            --query 'Credentials.[AccessKeyId,SecretAccessKey,SessionToken]' --output text" 2>&1
    }
    out=$(assume wrongaud)
    echo "$out" | grep -q 'InvalidParameterValue' && ok "a token for another audience is refused: $(echo "$out" | tr -s '\n' ' ' | cut -c1-160)" || bad "wrong audience: $out"
    K0=$(assume good)
    [ "$(echo "$K0" | wc -w | tr -d ' ')" = 3 ] && ok "a token for s3.csi.chert.us is exchanged (MinIO verified it against the cluster's issuer)" \
        || bad "exchange failed: '$(echo "$K0" | cut -c1-200)'"
    out=$(awsx "$K0" s3api put-object --bucket "$BUCKET" --key access/lean/_drill/o0-nopolicy.txt --body /etc/hostname)
    echo "$out" | grep -q ETag && ok "CONTROL: without a policy the role writes under the prefix" || bad "no-policy PUT: $(echo "$out" | tail -1)"
    [ "$(awsx "$K0" s3 cp "s3://$BUCKET/private/access-secret.txt" - 2>/dev/null)" = not-for-readers ] \
        && ok "CONTROL: without a policy the role reads another prefix" || bad "no-policy GET of another prefix failed"
    awsx "$ROOT_KEYS" s3api delete-object --bucket "$BUCKET" --key access/lean/_drill/o0-nopolicy.txt >/dev/null
else
    bad "sts-probe not ready"
fi
$K -n $NS delete pod sts-probe --wait=false >/dev/null 2>&1

leg O1 "the broker's sts backend is MinIO's STS (through a logging tap), with MinIO's role"
$K -n $SYS create configmap sts-tap-script --from-file=sts-tap.py=sts-tap.py --dry-run=client -o yaml | $K apply -f - >/dev/null
cat <<EOF | $K apply -f - >/dev/null
apiVersion: apps/v1
kind: Deployment
metadata: { name: sts-tap, namespace: $SYS }
spec:
  replicas: 1
  selector: { matchLabels: { app: sts-tap } }
  template:
    metadata: { labels: { app: sts-tap } }
    spec:
      containers:
        - name: tap
          image: python:3.12-alpine
          command: ["python3", "-u", "/tap/sts-tap.py"]
          env: [{ name: SHIM_UPSTREAM, value: "http://minio.$SYS.svc:9000/" }]
          ports: [{ containerPort: 8080 }]
          volumeMounts: [{ name: tap, mountPath: /tap }]
      volumes: [{ name: tap, configMap: { name: sts-tap-script } }]
---
apiVersion: v1
kind: Service
metadata: { name: sts-tap, namespace: $SYS }
spec:
  selector: { app: sts-tap }
  ports: [{ port: 8080, targetPort: 8080 }]
EOF
$K -n $SYS rollout restart deploy/sts-tap >/dev/null 2>&1
$K -n $SYS rollout status deploy/sts-tap --timeout=180s >/dev/null || bad "sts-tap did not roll out"
helm_broker --set broker.backend=sts --set broker.sts.url="http://sts-tap.$SYS.svc:8080/" --set broker.sts.roleArn="$ARN" \
    || bad "helm upgrade to the sts backend failed"
st=$(broker_status)
echo "$st" | grep -q '"backend":"sts"' && echo "$st" | grep -q '"readEnforcement":"sessionPolicy"' \
    && ok "broker /v1/status: backend sts, readEnforcement sessionPolicy" || bad "broker status: $st"
tap_log() { $K -n $SYS logs deploy/sts-tap 2>/dev/null | grep '"session"'; }

leg O2 "a mixed lean workspace: a writer and a reader, each keyed by MinIO's STS"
tpod ow editor workspace acc-lean false runc
tpod orr viewer workspace acc-lean true runc
OW=""; ORW=""
if wait_phase ow Running 420 && wait_phase orr Running 420; then
    ok "both pods Running (each checkout needed a key from MinIO's STS)"
    OW=$(wait_worker ow); ORW=$(wait_worker orr)
    got=$(wait_issued ow "readWrite none -" 150); [ "${got%% *}" = readWrite ] && ok "ow: issued '$got'" || bad "ow: issued '$got'"
    got=$(wait_issued orr "read sessionPolicy -" 150); [ "$got" = "read sessionPolicy -" ] && ok "orr: issued access=read enforcement=sessionPolicy" || bad "orr: issued '$got'"
    broker_log | grep -E ' issued' | grep -qiE 'error|refus' && bad "the broker logged an error beside an issue" || true
else
    bad "ow/orr not Running: ow $(mount_events ow | tail -1 | cut -c1-200) / orr $(mount_events orr | tail -1 | cut -c1-200)"
fi

leg O2t "the tap's record: the reader's exchange carried flint's read policy, the writer's none"
tl=$(tap_log); echo "$tl" > "$OUT/O-tap.log"
echo "$tl" | python3 -c "
import json, sys
rows = [json.loads(l) for l in sys.stdin if l.strip()]
ok = [r for r in rows if r['status'] == 200]
with_p = [r for r in ok if r['policy']]
without = [r for r in ok if not r['policy']]
print(len(rows), len(ok), len(with_p), len(without))
json.dump(with_p[-1]['policy'] if with_p else None, open('$WORK/tapped-policy.json', 'w'))
" > "$WORK/tap-counts" 2>/dev/null
read -r n_all n_ok n_with n_without < "$WORK/tap-counts"
[ "${n_with:-0}" -ge 1 ] && [ "${n_without:-0}" -ge 1 ] && [ "$n_all" = "$n_ok" ] \
    && ok "the tap saw $n_all exchanges, all 200: $n_with with a policy, $n_without without" \
    || bad "tap: all=$n_all ok=$n_ok with=$n_with without=$n_without"
pol=$(python3 -c "import json; print(json.load(open('$WORK/tapped-policy.json')))" 2>/dev/null)
cp "$WORK/tapped-policy.json" "$OUT/O2t-tapped-policy.json" 2>/dev/null
got=$(printf '%s' "$pol" | python3 -c "
import json, sys
d = json.load(sys.stdin)
acts = sorted(a for st in d['Statement'] for a in st['Action'])
res = sorted(str(st['Resource']) for st in d['Statement'])
print(','.join(acts), '|', ','.join(res))" 2>/dev/null)
want="s3:GetObject,s3:GetObjectVersion,s3:ListBucket,s3:ListBucketVersions | arn:aws:s3:::$BUCKET,arn:aws:s3:::$BUCKET/access/lean/*"
[ "$got" = "$want" ] && ok "the policy flint sent: reads only, on $BUCKET/access/lean/* (no s3:GetObjectAttributes)" \
    || bad "the tapped policy is '$got', want '$want'"

leg O3 "the keys each worker HOLDS: the reader's read one prefix, the writer's write"
if [ -n "$ORW" ] && [ -n "$OW" ]; then
    RK=$(wcreds "$ORW"); WK=$(wcreds "$OW")
    [ "$(echo "$RK" | wc -w | tr -d ' ')" = 3 ] && ok "the reader's worker holds session keys" || bad "reader keys: '$(echo "$RK" | wc -w)' words"
    out=$(awsx "$RK" s3api put-object --bucket "$BUCKET" --key access/lean/_drill/orr-put.txt --body /etc/hostname)
    denied "$out" && ok "reader's keys: PUT under the prefix denied" || bad "reader PUT: $(echo "$out" | tail -1)"
    out=$(awsx "$RK" s3api delete-object --bucket "$BUCKET" --key access/lean/_drill/seed.txt)
    denied "$out" && ok "reader's keys: DELETE under the prefix denied" || bad "reader DELETE: $(echo "$out" | tail -1)"
    [ "$(awsx "$RK" s3 cp "s3://$BUCKET/access/lean/_drill/seed.txt" - 2>/dev/null)" = under-the-prefix ] \
        && ok "reader's keys: GET under the prefix allowed" || bad "reader GET under the prefix failed"
    out=$(awsx "$RK" s3 cp "s3://$BUCKET/private/access-secret.txt" -)
    denied "$out" && ok "reader's keys: GET of another prefix denied" || bad "reader GET other: $(echo "$out" | tail -1)"
    out=$(awsx "$WK" s3api put-object --bucket "$BUCKET" --key access/lean/_drill/ow-put.txt --body /etc/hostname)
    echo "$out" | grep -q ETag && ok "CONTROL: the writer's keys PUT under the prefix" || bad "writer PUT: $(echo "$out" | tail -1)"
    awsx "$ROOT_KEYS" s3api delete-object --bucket "$BUCKET" --key access/lean/_drill/ow-put.txt >/dev/null
else
    bad "no workers to read keys from"
fi

leg O4 "the writer's publish reaches the reader"
t=$(date +%s)
if [ "$(twrite ow /workspace/o4.txt "o4-$t")" = ok ] && [ "$(tpublish ow o4-$t 120)" = ok ]; then
    ok "ow wrote and published o4.txt"
    wait_content orr /workspace/o4.txt "o4-$t" 120 && ok "orr reads o4.txt" || bad "orr: o4.txt is '$(tcat orr /workspace/o4.txt)'"
else
    bad "ow could not write and publish"
fi
r=$(twrite orr /workspace/orr.txt x); echo "$r" | grep -qi 'read-only file system' && ok "orr: EROFS" || bad "orr write: '$r'"

leg O5 "passthrough on a read grant: mount-s3 reads under the policy without GetObjectAttributes"
tpod pw editor mount acc-pt-local false runc
tpod pr viewer mount acc-pt-local false runc
if wait_phase pw Running 420 && wait_phase pr Running 420; then
    r=$(twrite pw /workspace/from-pw.txt "pw-$t"); [ "$r" = ok ] && ok "pw writes through mount-s3" || bad "pw write: $r"
    got=$(wait_issued pr "read sessionPolicy -" 150); [ "$got" = "read sessionPolicy -" ] && ok "pr: issued access=read enforcement=sessionPolicy" || bad "pr: issued '$got'"
    wait_content pr /workspace/from-pw.txt "pw-$t" 60 && ok "pr reads pw's file through mount-s3 on the read grant" \
        || bad "pr cannot read from-pw.txt: '$(tcat pr /workspace/from-pw.txt)'"
    r=$(twrite pr /workspace/from-pr.txt x)
    [ "$r" != ok ] && ok "pr cannot write: $r" || bad "pr wrote through a read grant"
else
    bad "pw/pr not Running: $(mount_events pr | tail -1 | cut -c1-200)"
fi
SAVE_AS=O-broker.log save broker_log
for p in ow orr pw pr; do w=$(worker_of "$p"); [ -n "$w" ] && $K -n $WNS logs "$w" > "$OUT/O-worker-$p.log" 2>/dev/null; done
clear_local
fi

if echo " $ARMS " | grep -q " G "; then
# ══ ARM G: gVisor ═════════════════════════════════════════════════════
leg G1 "F7 under runsc: 1000 files, an immediate publish, every file cited at its size"
tpod fg editor workspace acc-lean false gvisor
tpod fc editor workspace acc-lean false runc
tpod fr viewer workspace acc-lean true runc
tpod gr viewer workspace acc-lean true gvisor
if wait_phase fg Running 420 && wait_phase fc Running 420 && wait_phase fr Running 420 && wait_phase gr Running 420; then
    [ "$(kernel_of fg)" = gvisor ] && [ "$(kernel_of gr)" = gvisor ] && [ "$(kernel_of fc)" = host ] && [ "$(kernel_of fr)" = host ] \
        && ok "PRECONDITION: fg and gr run on gVisor's kernel, fc and fr on the host's" \
        || bad "PRECONDITION: kernels fg=$(kernel_of fg) gr=$(kernel_of gr) fc=$(kernel_of fc) fr=$(kernel_of fr)"
    for arm in g c; do
        w=f$arm; d=f7$arm-$RUN
        [ "$arm" = c ] && leg G1c "the same from a runc writer (F7's control)"
        [ -z "$(tdigest fr $d)" ] && ok "fr has no $d before $w publishes (the convergence check can fail)" || bad "fr already has $d"
        r=$(tfiles $w $d 1000); [ "$r" = ok ] || bad "$w wrote: $r"
        a=$(tpublish $w "f7-$arm-$(date +%s)" 180)
        [ "$a" = ok ] && ok "$w: 1000 files written, publish touched at once, ack ok" || bad "$w: ack '$a'"
        n=$(cited_at_size access/lean $d 1000)
        [ "$n" = 1000 ] && ok "the manifest cites all 1000 of $w's files at the size written" || bad "the manifest cites ${n:-0}/1000 of $d at size"
        want=$(tdigest $w $d)
        wait_digest fr $d "$want" 180 && ok "fr (runc reader) has $w's 1000 files byte for byte ($want)" || bad "fr digest '$(tdigest fr $d)' != $w's $want"
    done

    leg G2 "a runsc reader: EROFS in the tree and in .flint/, and it reads the writer's bytes"
    r=$(twrite gr /workspace/gr.txt x); echo "$r" | grep -qi 'read-only file system' && ok "gr: EROFS in the tree" || bad "gr tree write: '$r'"
    r=$(twrite gr /workspace/.flint/gr-probe x); echo "$r" | grep -qi 'read-only file system' && ok "gr: EROFS in .flint/" || bad "gr .flint write: '$r'"
    wait_digest gr "f7g-$RUN" "$(tdigest fg "f7g-$RUN")" 180 && ok "gr reads fg's 1000 files byte for byte" || bad "gr digest '$(tdigest gr "f7g-$RUN")'"
    r=$(twrite fg /workspace/fg-ctl.txt x); [ "$r" = ok ] && ok "CONTROL: fg (runsc writer) writes" || bad "fg write: '$r'"

    leg G3 "the runsc reader's host bind and its gofer's mount are ro; the runsc writer's are rw"
    hb=$(host_bind gr); case "$hb" in ro,*) ok "gr: host bind '$hb'" ;; *) bad "gr: host bind '$hb'" ;; esac
    hb=$(host_bind fg); case "$hb" in rw,*) ok "CONTROL fg: host bind '$hb'" ;; *) bad "fg: host bind '$hb'" ;; esac
    gv=$(gofer_view gr); case "$gv" in ro,*) ok "gr: the gofer serves /workspace from a mount '$gv'" ;; *) bad "gr: gofer mount '$gv'" ;; esac
    gv=$(gofer_view fg); case "$gv" in rw,*) ok "CONTROL fg: the gofer's mount '$gv'" ;; *) bad "fg: gofer mount '$gv'" ;; esac
else
    bad "G tenants not Running: fg $(mount_events fg | tail -1 | cut -c1-160) / gr $(mount_events gr | tail -1 | cut -c1-160)"
fi

leg G4 "the UDS door: reachable from a runc tenant, refused to a runsc tenant"
tpod dg editor workspace acc-door false gvisor python:3.12-alpine
tpod dr editor workspace acc-door false runc python:3.12-alpine
if wait_phase dg Running 420 && wait_phase dr Running 420; then
    connect() { $K -n $NS exec "$1" -c agent -- python3 -c '
import socket
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
try:
    s.connect("/workspace/.flint-sync/ctl.sock"); print("connected")
except Exception as e:
    print(type(e).__name__, e)' 2>&1; }
    for p in dg dr; do
        u=$(pod_uid $p); i=0
        while [ $i -lt 60 ] && ! onnode "test -S /var/lib/kubelet/pods/$u/volumes/kubernetes.io~csi/ws/mount/.flint-sync/ctl.sock"; do sleep 3; i=$((i + 3)); done
        onnode "test -S /var/lib/kubelet/pods/$u/volumes/kubernetes.io~csi/ws/mount/.flint-sync/ctl.sock" \
            && ok "PRECONDITION: $p's syncer bound ctl.sock in its own tree (host side)" || bad "PRECONDITION: no ctl.sock in $p's tree"
    done
    c=$(connect dr); [ "$c" = connected ] && ok "CONTROL: dr (runc) connects to the door" || bad "dr: '$c'"
    c=$(connect dg); case "$c" in connected) bad "dg (runsc) reached a host socket" ;; *Refused*|*Errno*) ok "dg (runsc) is refused: $c" ;; *) bad "dg: '$c'" ;; esac
    $K -n $NS exec dg -c agent -- ls -la /workspace/.flint-sync/ctl.sock > "$OUT/G4-dg-sees-socket.txt" 2>&1
else
    bad "door tenants not Running"
fi
SAVE_AS=G-broker.log save broker_log
for p in fg fc fr gr dg dr; do w=$(worker_of "$p"); [ -n "$w" ] && $K -n $WNS logs "$w" > "$OUT/G-worker-$p.log" 2>/dev/null; done
clear_local
fi

if echo " $ARMS " | grep -q " I "; then
# ══ ARM I: AccessIsolation ════════════════════════════════════════════
leg I1 "the lean operator's AccessIsolation per identity mode"
$K -n $SYS create secret generic lean-operator-creds --from-literal=AWS_ACCESS_KEY_ID=drill \
    --from-literal=AWS_SECRET_ACCESS_KEY=drillsecret --from-literal=AWS_REGION="$S3_REGION" --dry-run=client -o yaml | $K apply -f - >/dev/null
helm --kube-context "$CTX" upgrade --install flint-lean "$REPO/flint-lean-chart" -n $SYS \
    --set image.ref="dilipdalton/flint-lean-operator:$TAG" --set image.pullPolicy=Never \
    --set operatorCredentialsSecret=lean-operator-creds --set endpoint="$S3_ENDPOINT" >/dev/null \
    || bad "helm install flint-lean failed"
$K -n $SYS rollout status deploy/flint-lean --timeout=300s >/dev/null 2>&1 || bad "the lean operator did not roll out"
expect() { # workspace status reason
    local i=0 s r
    while [ $i -lt 180 ]; do s=$(cond "$1" status); r=$(cond "$1" reason); [ "$s $r" = "$2 $3" ] && break; sleep 3; i=$((i + 3)); done
    [ "$s $r" = "$2 $3" ] && ok "$1: AccessIsolation $s/$r" || bad "$1: AccessIsolation '$s/$r', want $2/$3"
}
expect iso-static False Cooperative
expect iso-ambient False Cooperative
expect iso-broker Unknown DecidedByBroker
expect iso-webidentity Unknown DecidedByBroker
expect iso-default Unknown DecidedByBroker
n=$($K -n $NS get flintleanworkspace iso-noconsumers -o jsonpath='{.status.conditions[*].type}' 2>/dev/null)
if [ -z "$n" ]; then
    bad "iso-noconsumers has no conditions at all — the operator never reconciled it, so an absent AccessIsolation says nothing"
else
    echo " $n " | grep -q ' AccessIsolation ' && bad "iso-noconsumers has AccessIsolation" || ok "iso-noconsumers: no AccessIsolation, beside [$n]"
fi

leg I2 "a mode change moves the condition"
before=$(cond iso-static lastTransitionTime)
sleep 2
$K -n $NS patch flintleanworkspace iso-static --type merge -p '{"spec":{"identity":{"mode":"broker"}}}' >/dev/null
expect iso-static Unknown DecidedByBroker
after=$(cond iso-static lastTransitionTime)
[ -n "$before" ] && [ "$after" != "$before" ] && ok "lastTransitionTime moved: $before → $after" || bad "lastTransitionTime '$before' → '$after'"
$K -n $NS patch flintleanworkspace iso-static --type merge -p '{"spec":{"identity":{"mode":"static"}}}' >/dev/null
$K -n $NS get flintleanworkspaces -o yaml > "$OUT/I-workspaces.yaml" 2>/dev/null
$K -n $SYS logs deploy/flint-lean > "$OUT/I-operator.log" 2>/dev/null
fi

echo
want_legs=""
echo " $ARMS " | grep -q " O " && want_legs="$want_legs O0 O1 O2 O2t O3 O4 O5"
echo " $ARMS " | grep -q " G " && want_legs="$want_legs G1 G1c G2 G3 G4"
echo " $ARMS " | grep -q " I " && want_legs="$want_legs I1 I2"
for want in $want_legs; do echo " $RAN_LEGS " | grep -q " $want " || bad "leg $want never ran"; done
echo "════════════════════════════════════════"
echo "flint read access on the local kind rig: $PASS ok, $FAILED bad, $SKIPPED skipped"
[ "$FAILED" = "0" ]
