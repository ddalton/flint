#!/usr/bin/env bash
# s3.csi.chert.us across TWO clusters, ONE S3 endpoint — the multi-cluster
# use case the user named: agents on different clusters working the same
# project's artifacts (docs/plans/csi-node-mount-design.md §0).
#
# Two kind clusters, each with its own flint-s3-csi install (chart,
# broker, CRDs, CRs), and one MinIO running OUTSIDE both as a plain
# container on the `kind` docker network — the "single s3 endpoint".
# Nothing in flint spans the clusters; the bucket prefix is the meeting
# point, and every leg below is about what that does and does not give:
#
#   M1  passthrough: an object written on cluster 1 is read on cluster 2
#       with the same bytes, and vice versa — S3 is the medium; a pod on
#       another prefix on cluster 2 sees none of it (control).
#   M2  identity is PER CLUSTER: a pod-bound token minted by cluster 1 is
#       refused by cluster 2's broker at TokenReview (400
#       InvalidIdentityToken), while cluster 2's own token is a valid
#       identity refused only for its missing registration (403
#       AccessDenied) — the control that proves the refusal is the trust
#       root, not the transport.
#   M3  lean: a project seeded and published on cluster 1 is checked out,
#       complete and byte-correct, by a cold pod on cluster 2; a file
#       written on cluster 2 is drained into the bucket and seen by the
#       next pod on cluster 1. AND the exclusivity rule (§0 point 4): while
#       cluster 1's pod holds the workspace lease, cluster 2's pod stays
#       ContainerCreating with an event naming the checkout in progress,
#       and proceeds only once cluster 1's pod is gone.
#
# House rules as in run-s3csi.sh: every leg proves its precondition or
# FAILS; every refusal has an accepted control; readers are uid 1001;
# every read asserts CONTENT; the roster fails the run if a leg never
# ran.
#
#   ./run-multi.sh setup      # MinIO container + two kind clusters + chart in each
#   ./run-multi.sh            # the legs
#   ./run-multi.sh teardown
#
# Images: ../build-images.sh first (TAG=dev, loaded into BOTH clusters by
# setup). Values overridable: TAG, C1, C2 (kind cluster names).
set -u
cd "$(dirname "$0")"
REPO=$(cd ../../.. && pwd)
TAG=${TAG:-dev}
C1=${C1:-flint-s3csi-m1}
C2=${C2:-flint-s3csi-m2}
# THE SUBSTRATE (as in run-s3csi.sh). STORE=minio is the kind rig: two
# kind clusters this script creates, one MinIO container on the docker
# network. STORE=s3 is two EXISTING clusters (X1/X2 name their kubectl
# contexts) and a real bucket — BUCKET, S3_REGION, S3_ENDPOINT, and a
# key from S3_KEY_FILE or AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY; the
# images must have been PUSHED under TAG. Nothing in the legs knows
# which: the bucket prefix is the meeting point either way.
STORE=${STORE:-minio}
X1=${X1:-kind-$C1}; X2=${X2:-kind-$C2}
NS=s3-tenants; WNS=flint-workers; SYS=flint-system
MINIO=flint-s3-minio
BUCKET=${BUCKET:-s3bucket}
S3_REGION=${S3_REGION:-us-east-1}
S3_ENDPOINT=${S3_ENDPOINT:-}
S3_KEY=drill; S3_SECRET=drillsecret
if [ "$STORE" = s3 ]; then
    [ "$BUCKET" != s3bucket ] || { echo "STORE=s3 needs BUCKET" >&2; exit 2; }
    [ -n "$S3_ENDPOINT" ] || S3_ENDPOINT="https://s3.$S3_REGION.amazonaws.com"
    if [ -n "${S3_KEY_FILE:-}" ]; then
        S3_KEY=$(jq -r .AccessKey.AccessKeyId "$S3_KEY_FILE"); S3_SECRET=$(jq -r .AccessKey.SecretAccessKey "$S3_KEY_FILE")
    else
        S3_KEY=${AWS_ACCESS_KEY_ID:-}; S3_SECRET=${AWS_SECRET_ACCESS_KEY:-}
    fi
    [ -n "$S3_KEY" ] && [ -n "$S3_SECRET" ] || { echo "STORE=s3 needs S3_KEY_FILE or AWS_ACCESS_KEY_ID + AWS_SECRET_ACCESS_KEY" >&2; exit 2; }
fi

PASS=0; FAILED=0; RAN_LEGS=""
bad()  { echo "  BAD: $1"; FAILED=$((FAILED + 1)); }
ok()   { PASS=$((PASS + 1)); echo "  ok: $1"; }
note() { echo "  NOTE: $1"; }
leg()  { RAN_LEGS="$RAN_LEGS $1"; echo; echo "── $1 — $2"; }

# ── the shared endpoint ──────────────────────────────────────────────
minio_ip() { docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "$MINIO" 2>/dev/null; }
# mc as a throwaway container on the same network; alias `m` from env.
# -i is load-bearing: `mc pipe` reads the object body from STDIN, and a
# `docker run` without it seeds every object EMPTY — which reads back as
# a successful mount serving zero bytes, the most misleading green there
# is (measured: shard-05.txt was empty on both clusters while the leg
# that wrote its own file passed).
mcx() {
    if [ "$STORE" = s3 ]; then
        # Through cluster 1's mc pod, with the workers' own key.
        kubectl --context "$X1" -n $SYS exec -i mc-s3 -- mc "$@" 2>/dev/null
    else
        docker run --rm -i --network kind -e "MC_HOST_m=http://drill:drillsecret@$(minio_ip):9000" minio/mc "$@" 2>/dev/null
    fi
}
# One cluster's fixtures, rendered for the store.
render() { sed -e "s#__ENDPOINT__#$1#g" -e "s#__BUCKET__#$BUCKET#g" -e "s#__REGION__#$S3_REGION#g" -e "s#__KEY__#$S3_KEY#g" -e "s#__SECRET__#$S3_SECRET#g" cluster.yaml.tpl; }
seed() {
    for i in 00 01 02 03 04 05 06 07 08 09 10; do echo "seeded-object-$i" | mcx pipe "m/$BUCKET/datasets/imagenet/shard-$i.txt" >/dev/null; done
    echo "deep-seeded" | mcx pipe "m/$BUCKET/datasets/imagenet/sub/deep.txt" >/dev/null
    echo "elsewhere-only" | mcx pipe "m/$BUCKET/elsewhere/only.txt" >/dev/null
    echo "must-not-be-visible" | mcx pipe "m/$BUCKET/private/secret.txt" >/dev/null
    # 11 shards + sub/deep.txt = 12. The check said 11 — the shard count
    # alone — from the day the file was written, and since it exits hard
    # before a single leg, `setup` had NEVER completed on kind: the EC2
    # run (2026-09-04) was the first time this rig got past its own seed
    # gate. Fail-closed, not vacuous — no kind multi-cluster number was
    # ever produced, wrong or right. MinIO lists twelve too (measured).
    n=$(lcount datasets/imagenet/)
    got=$(lobj datasets/imagenet/shard-05.txt)
    [ "$n" = "12" ] && [ "$got" = "seeded-object-05" ] || {
        echo "SEED FAILED: $n objects under datasets/imagenet/, shard-05.txt='$got' (expected 12 and seeded-object-05)" >&2
        exit 2; }
    echo "seeded and verified: $n objects under datasets/imagenet/, shard-05.txt=$got"
}
install_chart() { # ctx endpoint
    kc "$1" apply -f "$REPO/flint-passthrough-chart/crds/flintpassthroughmounts.yaml" -f "$REPO/flint-lean-chart/crds/flintleanworkspaces.yaml" >/dev/null
    render "$2" | kc "$1" apply -f - >/dev/null
    helm --kube-context "$1" upgrade --install flint-s3-csi "$REPO/flint-s3-csi-chart" -n $SYS \
        --set node.image.tag="$TAG" --set workers.passthroughImage.tag="$TAG" --set workers.leanImage.tag="$TAG" \
        --set node.image.pullPolicy=IfNotPresent --set node.region="$S3_REGION" \
        --set broker.backend=static --set broker.static.secretRef=s3-broker-static --set broker.replicas=1 >/dev/null
    kc "$1" -n $SYS rollout status ds/flint-s3-csi-node --timeout=180s >/dev/null
    kc "$1" -n $SYS rollout status deploy/flint-s3-broker --timeout=180s >/dev/null
}
lobj()   { mcx cat "m/$BUCKET/$1"; }
lcount() { mcx ls --recursive "m/$BUCKET/$1" | grep -c . ; }
# Resolve the manifest THROUGH the pointer: `.flint/lean/current` is the
# only mutable metadata object and it NAMES the write-once generation
# holding the entries. The pre-pointer `.flint/lean/manifest` key is a
# fallback for a bucket an older binary wrote, and is tried LAST — after
# migration it holds a refusal doc with no `.entries` at all.
#
# Reading the legacy key first is not a stale path, it is a VACUOUS one:
# these helpers answer 0/false for a missing object, so "seq unchanged"
# and "entry count unchanged" pass by both sides being nothing.
lmbody() {
    local c k addrs a body all
    c=$(lobj "$1/.flint/lean/current")
    if [ -n "$c" ]; then
        # Three layouts, newest first — a CHUNK LIST (`.chunks`, the
        # default since 2026-09-03), one generation object
        # (`.entries_key`), the pre-pointer single key — each tested
        # POSITIVELY. This copy knew only the middle one until the EC2
        # run: M3's citation poll read `null` for a chunked pointer and
        # reported late-m2.txt uncited for 120 s while the very next
        # assertions read it, with its bytes, on the other cluster.
        if printf '%s' "$c" | jq -e 'has("chunks")' >/dev/null 2>&1; then
            addrs=$(printf '%s' "$c" | jq -r '.chunks[].addr')
            all='{"entries":{}}'
            for a in $addrs; do
                body=$(lobj "$1/.flint/lean/chunks/$a")
                if [ -z "$body" ]; then
                    echo "  NOTE: pointer for $1 names chunk $a, which the bucket does not have" >&2
                    return 1
                fi
                all=$(printf '%s\n%s' "$all" "$body" | jq -s '{entries: (.[0].entries + .[1].entries)}')
            done
            printf '%s' "$all"
            return
        fi
        k=$(printf '%s' "$c" | jq -r '.entries_key // empty')
        [ -z "$k" ] && return 1
        lobj "$k"
        return
    fi
    lobj "$1/.flint/lean/manifest"
}
lmhas()  { local m; m=$(lmbody "$1"); [ -z "$m" ] && return 1; printf '%s' "$m" | jq -e --arg p "$2" '.entries | has($p)' >/dev/null; }
# The FENCING seq is the POINTER's: a takeover rotation bumps it and
# leaves `entries_seq` alone, which is the point of the layout.
lmseq()  { local c m; c=$(lobj "$1/.flint/lean/current"); [ -n "$c" ] && { printf '%s' "$c" | jq -r '.seq // 0'; return; }; m=$(lobj "$1/.flint/lean/manifest"); [ -z "$m" ] && { echo 0; return; }; printf '%s' "$m" | jq -r '.seq // 0'; }

# ── per-cluster helpers: every one takes the CONTEXT first ───────────
kc()      { local x=$1; shift; kubectl --context "$x" "$@"; }
# exec can fail for a second or two after a pod turns Running (the
# container's streaming endpoint is not up yet): retry on a failed exec.
inpod()   { local x=$1 p=$2 try out rc; shift 2; for try in 1 2 3; do out=$(kc "$x" -n $NS exec "$p" -c agent -- /bin/sh -c "$*" 2>/dev/null); rc=$?; [ $rc -eq 0 ] && break; sleep 2; done; [ -n "$out" ] && printf '%s\n' "$out"; return $rc; }
# ...and for CONTENT reads, retry until there IS output: a raced exec
# returns empty with exit 0 and would read as "the file is empty", which
# is exactly the wrong verdict for a cross-cluster content assertion.
inpod_out() { local x=$1 p=$2 try out; shift 2; for try in 1 2 3 4 5; do out=$(inpod "$x" "$p" "$@"); [ -n "$out" ] && break; sleep 3; done; [ -n "$out" ] && printf '%s\n' "$out"; }
phase()   { kc "$1" -n $NS get pod "$2" -o jsonpath='{.status.phase}' 2>/dev/null; }
wait_phase() { local x=$1 p=$2 want=$3 secs=$4 i=0; while [ $i -lt "$secs" ]; do [ "$(phase "$x" "$p")" = "$want" ] && return 0; sleep 2; i=$((i + 2)); done; return 1; }
mount_events() { kc "$1" -n $NS get events --field-selector involvedObject.name="$2" -o jsonpath='{range .items[*]}{.reason}: {.message}{"\n"}{end}' 2>/dev/null; }
require_pod() { local ph; ph=$(phase "$1" "$2"); [ "$ph" = "Running" ] && return 0; bad "[$1] pod $2 is '${ph:-absent}', not Running — every observation this leg makes would be an empty string"; return 1; }
apply_pod() { # ctx name — one pod out of pods.yaml / lean-pods.yaml
    python3 - "$2" <<'PY' | kc "$1" apply -f - >/dev/null
import sys,yaml
want=sys.argv[1]
for f in ("pods.yaml","lean-pods.yaml"):
    for d in yaml.safe_load_all(open(f)):
        if d and d.get("metadata",{}).get("name")==want:
            print(yaml.safe_dump(d)); sys.exit(0)
sys.exit("no pod named "+want)
PY
}
delete_pod() { kc "$1" -n $NS delete pod "$2" --ignore-not-found --wait=true --timeout=180s >/dev/null 2>&1; }

# ── setup / teardown ─────────────────────────────────────────────────
if [ "${1:-}" = "setup" ] && [ "$STORE" = s3 ]; then
    set -e
    for X in "$X1" "$X2"; do
        kubectl --context "$X" get nodes >/dev/null || { echo "context $X is not reachable" >&2; exit 2; }
        install_chart "$X" "$S3_ENDPOINT"
        echo "cluster $X: s3.csi.chert.us installed, CRs point at $S3_ENDPOINT/$BUCKET"
    done
    kc "$X1" -n $SYS wait --for=condition=ready pod/mc-s3 --timeout=120s >/dev/null
    # A versioned bucket keeps what the last run left; the legs count.
    mcx rm --recursive --force --versions "m/$BUCKET/" >/dev/null 2>&1 || true
    seed
    echo "setup done"
    exit 0
fi
if [ "${1:-}" = "teardown" ] && [ "$STORE" = s3 ]; then
    for X in "$X1" "$X2"; do
        kc "$X" -n $NS delete pods --all --ignore-not-found --wait=true --timeout=300s >/dev/null 2>&1 || true
        render "$S3_ENDPOINT" | kc "$X" delete -f - --ignore-not-found --wait=false >/dev/null 2>&1 || true
        helm --kube-context "$X" uninstall flint-s3-csi -n $SYS >/dev/null 2>&1 || true
        kc "$X" -n $WNS delete pod --all --force --grace-period=0 --ignore-not-found >/dev/null 2>&1 || true
    done
    exit 0
fi
if [ "${1:-}" = "setup" ]; then
    set -e
    if ! docker inspect "$MINIO" >/dev/null 2>&1; then
        docker run -d --name "$MINIO" --network kind -e MINIO_ROOT_USER=drill -e MINIO_ROOT_PASSWORD=drillsecret \
            minio/minio server /data --console-address :9001 >/dev/null
    fi
    for i in $(seq 1 30); do docker exec "$MINIO" curl -sf http://127.0.0.1:9000/minio/health/live >/dev/null 2>&1 && break; sleep 1; done
    IP=$(minio_ip); echo "MinIO (outside both clusters): http://$IP:9000"
    mcx mb --ignore-existing "m/$BUCKET" >/dev/null
    seed
    for C in "$C1" "$C2"; do
        X="kind-$C"
        kind get clusters 2>/dev/null | grep -qx "$C" || kind create cluster --name "$C" --wait 120s >/dev/null
        kind load docker-image --name "$C" "dilipdalton/flint-s3-csi:$TAG" "dilipdalton/flint-s3-worker:$TAG" "dilipdalton/flint-s3-worker-lean:$TAG" >/dev/null
        install_chart "$X" "http://$IP:9000"
        echo "cluster $C: s3.csi.chert.us installed, CRs point at http://$IP:9000"
    done
    echo "setup done"
    exit 0
fi
if [ "${1:-}" = "teardown" ]; then
    kind delete cluster --name "$C1" 2>/dev/null || true
    kind delete cluster --name "$C2" 2>/dev/null || true
    docker rm -f "$MINIO" >/dev/null 2>&1 || true
    exit 0
fi

if [ "$STORE" = s3 ]; then
    echo "s3.csi.chert.us multi-cluster e2e — $X1 + $X2, one bucket $BUCKET at $S3_ENDPOINT"
else
    IP=$(minio_ip)
    [ -n "$IP" ] || { echo "no $MINIO container — run setup"; exit 2; }
    echo "s3.csi.chert.us multi-cluster e2e — $C1 + $C2, one MinIO at $IP"
fi
for X in "$X1" "$X2"; do
    kc "$X" get csidriver s3.csi.chert.us >/dev/null 2>&1 || { echo "cluster $X has no s3.csi.chert.us — run setup"; exit 2; }
done

# ── M1 passthrough across clusters ───────────────────────────────────
leg M1 "passthrough: bytes written on cluster 1 are read on cluster 2 and back; another prefix sees none of it"
for X in "$X1" "$X2"; do delete_pod "$X" reader; apply_pod "$X" reader; done
apply_pod "$X2" reader-elsewhere
mcx rm "m/$BUCKET/datasets/imagenet/from-m1.txt" "m/$BUCKET/datasets/imagenet/from-m2.txt" >/dev/null 2>&1
if wait_phase "$X1" reader Running 240 && wait_phase "$X2" reader Running 240; then
    ok "reader Running on both clusters (each cluster's own plugin, broker and worker)"
    [ "$(inpod_out "$X1" reader cat /mnt/s3/shard-05.txt)" = "seeded-object-05" ] && [ "$(inpod_out "$X2" reader cat /mnt/s3/shard-05.txt)" = "seeded-object-05" ] \
        && ok "both clusters read the seeded shard-05.txt with its bytes" || bad "seeded bytes differ across clusters"
    inpod "$X1" reader "printf 'written on m1\n' > /mnt/s3/from-m1.txt" || bad "cluster 1 could not write"
    i=0; while [ $i -lt 60 ] && [ "$(inpod_out "$X2" reader cat /mnt/s3/from-m1.txt)" != "written on m1" ]; do sleep 3; i=$((i + 3)); done
    [ "$(inpod_out "$X2" reader cat /mnt/s3/from-m1.txt)" = "written on m1" ] && ok "cluster 2 reads cluster 1's object with its bytes (${i}s after the write)" || bad "cluster 2 never saw from-m1.txt: '$(inpod "$X2" reader cat /mnt/s3/from-m1.txt)'"
    inpod "$X2" reader "printf 'written on m2\n' > /mnt/s3/from-m2.txt" || bad "cluster 2 could not write"
    i=0; while [ $i -lt 60 ] && [ "$(inpod_out "$X1" reader cat /mnt/s3/from-m2.txt)" != "written on m2" ]; do sleep 3; i=$((i + 3)); done
    [ "$(inpod_out "$X1" reader cat /mnt/s3/from-m2.txt)" = "written on m2" ] && ok "cluster 1 reads cluster 2's object with its bytes (${i}s after the write)" || bad "cluster 1 never saw from-m2.txt"
    [ "$(lobj datasets/imagenet/from-m1.txt)" = "written on m1" ] && ok "the object is in the ONE bucket, not a cluster-local cache" || bad "from-m1.txt is not in the bucket"
    # CONTROL: same bucket, other prefix, other cluster — one file, no leak.
    if wait_phase "$X2" reader-elsewhere Running 240; then
        n=$(inpod_out "$X2" reader-elsewhere ls /mnt/s3 | wc -l | tr -d ' ')
        [ "$n" = "1" ] && [ "$(inpod_out "$X2" reader-elsewhere cat /mnt/s3/only.txt)" = "elsewhere-only" ] && ok "CONTROL: the other prefix on cluster 2 shows exactly its one file" || bad "CONTROL: other prefix shows $n entries"
        inpod "$X2" reader-elsewhere "test -f /mnt/s3/from-m1.txt" && bad "CONTROL: the other prefix sees from-m1.txt" || ok "CONTROL: from-m1.txt is invisible outside its prefix"
    else
        bad "reader-elsewhere never came up on cluster 2"
    fi
else
    bad "reader did not come up on both clusters: m1=$(phase "$X1" reader) m2=$(phase "$X2" reader) — $(mount_events "$X1" reader | tail -1 | cut -c1-160)"
fi

# ── M2 identity is per cluster ───────────────────────────────────────
leg M2 "a token minted by cluster 1 is refused by cluster 2's broker at TokenReview; cluster 2's own is refused only for its registration"
for X in "$X1" "$X2"; do delete_pod "$X" self-minter; apply_pod "$X" self-minter; done
if wait_phase "$X1" self-minter Running 120 && wait_phase "$X2" self-minter Running 120; then
    T1=$(inpod_out "$X1" self-minter cat /tok/token)
    [ -n "$T1" ] && ok "cluster 1 minted a pod-bound s3.csi.chert.us token ($(echo -n "$T1" | wc -c | tr -d ' ') bytes)" || bad "no token on cluster 1"
    resp=$(inpod_out "$X2" self-minter "wget -q -O - --post-data 'Action=AssumeRoleWithWebIdentity&Version=2011-06-15&RoleArn=arn:flint:iam::passthrough:role/datasets&RoleSessionName=foreign&WebIdentityToken=$T1' http://flint-s3-broker.$SYS.svc/ 2>&1 || true")
    blog=$(kc "$X2" -n $SYS logs deploy/flint-s3-broker --since=60s 2>/dev/null)
    echo "$resp" | grep -q '400' && echo "$blog" | grep -q 'InvalidIdentityToken' && ok "cluster 2's broker refused cluster 1's token: 400 InvalidIdentityToken (TokenReview against cluster 2's API server)" || bad "cluster 2 answered cluster 1's token with: $(echo "$resp" | head -c 160) / $(echo "$blog" | grep -i refused | tail -1 | cut -c1-160)"
    # CONTROL: cluster 2's OWN token is a valid identity there — refused for the registration, not the trust root.
    resp=$(inpod_out "$X2" self-minter "T=\$(cat /tok/token); wget -q -O - --post-data \"Action=AssumeRoleWithWebIdentity&Version=2011-06-15&RoleArn=arn:flint:iam::passthrough:role/datasets&RoleSessionName=own&WebIdentityToken=\$T\" http://flint-s3-broker.$SYS.svc/ 2>&1 || true")
    blog=$(kc "$X2" -n $SYS logs deploy/flint-s3-broker --since=60s 2>/dev/null)
    echo "$resp" | grep -q '403' && echo "$blog" | grep -q 'AccessDenied' && ok "CONTROL: cluster 2's own token is authenticated and refused 403 AccessDenied (no live publish registration)" || bad "CONTROL: own token answered: $(echo "$resp" | head -c 160)"
else
    bad "self-minter did not come up on both clusters"
fi
for X in "$X1" "$X2"; do kc "$X" -n $NS delete pod self-minter --wait=false >/dev/null 2>&1; done

# ── M3 lean across clusters, with the lease ──────────────────────────
leg M3 "lean: seeded on cluster 1, checked out on cluster 2, drained back, seen on cluster 1 — and one holder at a time"
for X in "$X1" "$X2"; do delete_pod "$X" lean-agent; delete_pod "$X" lean-seeder; done
mcx rm --recursive --force "m/$BUCKET/tenants/proj/" >/dev/null 2>&1
apply_pod "$X1" lean-seeder
if wait_phase "$X1" lean-seeder Running 300; then
    ok "cluster 1: lean-seeder Running (claimed the lease on the shared prefix)"
    i=0; while [ $i -lt 120 ] && ! kc "$X1" -n $NS logs lean-seeder 2>/dev/null | grep -q 'SEED PUBLISHED'; do sleep 2; i=$((i + 2)); done
    kc "$X1" -n $NS logs lean-seeder 2>/dev/null | grep -q 'SEED PUBLISHED' && ok "cluster 1: the declared publish was acked (${i}s)" || bad "cluster 1 seeder: $(kc "$X1" -n $NS logs lean-seeder 2>/dev/null | tail -1)"
    [ "$(lcount tenants/proj/files/src/)" = "200" ] && ok "the bucket holds 200 objects under tenants/proj/files/src/" || bad "bucket holds $(lcount tenants/proj/files/src/) objects, not 200"
    # EXCLUSIVITY: cluster 2's pod must WAIT while cluster 1 holds the lease.
    apply_pod "$X2" lean-agent
    if wait_phase "$X2" lean-agent Running 90; then
        bad "EXCLUSIVITY: cluster 2's lean-agent became Running while cluster 1's seeder still holds the workspace lease"
    else
        ev=$(mount_events "$X2" lean-agent)
        echo "$ev" | grep -q 'checkout of proj in progress' && ok "EXCLUSIVITY: cluster 2's pod stays ContainerCreating with an event naming the checkout in progress (the lease is held on cluster 1)" || bad "EXCLUSIVITY: cluster 2's pod is not Running but its events say: $(echo "$ev" | tail -1 | cut -c1-160)"
    fi
    # Release: delete cluster 1's seeder (drain, lease released) — cluster 2 proceeds.
    delete_pod "$X1" lean-seeder
    if wait_phase "$X2" lean-agent Running 300; then
        ok "cluster 2's lean-agent proceeded once cluster 1's pod was gone"
        [ "$(inpod_out "$X2" lean-agent cat /tmp/gate)" = "GATE-OK" ] && ok "cluster 2: the first instruction ran after checkout-complete" || bad "cluster 2: gate broken"
        [ "$(inpod_out "$X2" lean-agent cat /tmp/seen-count | tr -d ' ')" = "200" ] && ok "cluster 2: the first instruction saw all 200 files seeded on cluster 1" || bad "cluster 2 saw $(inpod "$X2" lean-agent cat /tmp/seen-count) files"
        [ "$(inpod_out "$X2" lean-agent cat /tmp/seen-sample)" = "unit 0042 of the seeded project" ] && ok "cluster 2: f0042.txt carries cluster 1's bytes" || bad "cluster 2: f0042.txt reads '$(inpod "$X2" lean-agent cat /tmp/seen-sample)'"
        # Drain back: a file written on cluster 2 reaches the bucket only through the delete.
        inpod "$X2" lean-agent "printf 'written on m2 after the seed\n' > /workspace/src/late-m2.txt" || bad "cluster 2 could not write late-m2.txt"
        sleep 5
        lmhas tenants/proj src/late-m2.txt && bad "PRECONDITION: late-m2.txt cited before the delete (a tick published it)" || ok "PRECONDITION: late-m2.txt is not cited before the delete (floorSecs is an hour)"
        delete_pod "$X2" lean-agent
        i=0; while [ $i -lt 120 ] && ! lmhas tenants/proj src/late-m2.txt; do sleep 2; i=$((i + 2)); done
        lmhas tenants/proj src/late-m2.txt && ok "cluster 2's drain published late-m2.txt (cited ${i}s after the delete) at seq $(lmseq tenants/proj)" || bad "late-m2.txt not cited 120 s after cluster 2's pod was deleted"
        # Back on cluster 1: a cold pod sees 201 files including cluster 2's.
        apply_pod "$X1" lean-agent
        if wait_phase "$X1" lean-agent Running 300; then
            [ "$(inpod_out "$X1" lean-agent cat /tmp/seen-count | tr -d ' ')" = "201" ] && ok "cluster 1: a cold pod sees 201 files — the 200 it seeded plus cluster 2's" || bad "cluster 1 sees $(inpod "$X1" lean-agent cat /tmp/seen-count) files, not 201"
            [ "$(inpod_out "$X1" lean-agent cat /workspace/src/late-m2.txt)" = "written on m2 after the seed" ] && ok "cluster 1 reads cluster 2's late-m2.txt with its bytes" || bad "cluster 1: late-m2.txt reads '$(inpod "$X1" lean-agent cat /workspace/src/late-m2.txt)'"
            delete_pod "$X1" lean-agent
        else
            bad "cluster 1's lean-agent never became Running: $(mount_events "$X1" lean-agent | tail -1 | cut -c1-160)"
        fi
    else
        bad "cluster 2's lean-agent never became Running after cluster 1 released: $(mount_events "$X2" lean-agent | tail -1 | cut -c1-160)"
    fi
else
    bad "cluster 1's lean-seeder never became Running: $(mount_events "$X1" lean-seeder | tail -2 | cut -c1-200)"
fi
for X in "$X1" "$X2"; do kc "$X" -n $NS delete pod reader reader-elsewhere lean-agent lean-seeder --ignore-not-found --wait=false >/dev/null 2>&1; done

# ── roster ────────────────────────────────────────────────────────────
echo
for want in M1 M2 M3; do echo " $RAN_LEGS " | grep -q " $want " || bad "leg $want never ran"; done
echo "════════════════════════════════════════"
echo "s3.csi.chert.us multi-cluster e2e: $PASS ok, $FAILED bad"
[ "$FAILED" = "0" ]
