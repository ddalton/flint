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
# THE SUBSTRATE. The legs are the same on kind and on real nodes; four
# things differ, and each is a knob rather than a fork of this file:
#   STORE=minio|s3   the object store — the rig's in-cluster MinIO, or a
#                    real bucket: BUCKET, S3_REGION, S3_ENDPOINT (default
#                    the region's S3), and a key from S3_KEY_FILE (the
#                    JSON `aws iam create-access-key` prints) or from
#                    AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY.
#   NODE_EXEC=docker|nodesh   how `onnode` reaches the node's host: docker
#                    exec into the kind node, or scripts/nodesh.sh (a
#                    privileged pod that nsenters PID 1) on a cluster the
#                    Mac cannot ssh into. nodesh needs KUBECONFIG set.
#   NODE             the node the legs drain, roll and inspect: default
#                    the first WORKER (no control-plane role), else the
#                    first node — on kind the only one.
#   TAG              the image tag; on a real cluster the images must
#                    have been PUSHED (build-images.sh PUSH=1 ARCH=amd64).
STORE=${STORE:-minio}
BUCKET=${BUCKET:-s3bucket}
S3_REGION=${S3_REGION:-us-east-1}
S3_ENDPOINT=${S3_ENDPOINT:-http://minio.flint-system.svc:9000}
NODE_EXEC=${NODE_EXEC:-docker}
if [ "$STORE" = s3 ]; then
    [ "$BUCKET" != s3bucket ] || { echo "STORE=s3 needs BUCKET (a real bucket name)" >&2; exit 2; }
    [ "$S3_ENDPOINT" != http://minio.flint-system.svc:9000 ] || S3_ENDPOINT="https://s3.$S3_REGION.amazonaws.com"
    if [ -n "${S3_KEY_FILE:-}" ]; then
        AWS_ACCESS_KEY_ID=$(jq -r .AccessKey.AccessKeyId "$S3_KEY_FILE")
        AWS_SECRET_ACCESS_KEY=$(jq -r .AccessKey.SecretAccessKey "$S3_KEY_FILE")
    fi
    [ -n "${AWS_ACCESS_KEY_ID:-}" ] && [ -n "${AWS_SECRET_ACCESS_KEY:-}" ] \
        || { echo "STORE=s3 needs S3_KEY_FILE or AWS_ACCESS_KEY_ID + AWS_SECRET_ACCESS_KEY" >&2; exit 2; }
fi
NODE=${NODE:-$($K get nodes -l '!node-role.kubernetes.io/control-plane' -o jsonpath='{.items[0].metadata.name}' 2>/dev/null)}
[ -n "$NODE" ] || NODE=$($K get nodes -o jsonpath='{.items[0].metadata.name}' 2>/dev/null)

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
# `inpod` retries, because most legs read a transient exec failure as
# noise. S12 does not: it asks questions whose answer is legitimately
# FALSE (`test -f` on a file that must not exist yet), so it needs the
# command's own exit status, once.
tsh()     { local p=$1; shift; $K -n $NS exec "$p" -c agent -- /bin/sh -c "$*" >/dev/null 2>&1; }
tsh_out() { local p=$1; shift; $K -n $NS exec "$p" -c agent -- /bin/sh -c "$*" 2>/dev/null; }
mcx()    { $K -n $SYS exec mc-s3 -- "$@" 2>/dev/null; }
onnode() {
    case "$NODE_EXEC" in
        nodesh) "$REPO/scripts/nodesh.sh" "$NODE" "$*" 2>/dev/null ;;
        *)      docker exec "$NODE" sh -c "$*" 2>/dev/null ;;
    esac
}
# THE plugin pod: the one on $NODE. A DaemonSet has one per node, and on
# a cluster with more than one node `items[0]` is whichever sorts first —
# the first EC2 run's S17 rolled the worker's plugin and then read the
# CONTROL PLANE's plugin for the adoption line and the start time, so it
# saw "neither branch" and a marker "younger than the new plugin" that
# was S9's restart of a pod it never touched.
plugin_pod() { $K -n $SYS get pods -l app.kubernetes.io/name=flint-s3-csi-node --field-selector spec.nodeName="$NODE" -o jsonpath='{.items[0].metadata.name}' 2>/dev/null; }
# Kill a worker's CONTAINER from the host, the death §6.1 fears: not an
# API delete (the admission policy refuses it from anyone but the plugin,
# and a graceful delete is a different death). On the kind node that is
# `crictl stop -t 0`; a real node may carry no crictl at all (Amazon
# Linux 2023 kubeadm nodes ship `ctr` only — the first EC2 run's S9 found
# the stop failing and the control vacuous), so the fallback is SIGKILL
# to every process in the pod's cgroup except the sandbox's pause — the
# runtime sees the container die exactly as it would under crictl. The
# pod uid appears in the cgroup path with `-` (cgroupfs driver) or `_`
# (systemd driver); both are compared with the separators stripped.
kill_worker() {
    local uid u
    uid=$($K -n $WNS get pod "$1" -o jsonpath='{.metadata.uid}' 2>/dev/null)
    u=$(printf '%s' "$uid" | tr -d '-')
    [ -n "$u" ] || return 1
    onnode "if command -v crictl >/dev/null 2>&1; then crictl --timeout 60s stop -t 0 \$(crictl ps -q --pod \$(crictl pods --name $1 -q)) >/dev/null 2>&1; fi; \
            for p in /proc/[0-9]*; do [ \"\$(cat \$p/comm 2>/dev/null)\" = pause ] && continue; \
              case \"\$(tr -d '_-' < \$p/cgroup 2>/dev/null)\" in *pod$u*) kill -9 \${p#/proc/} 2>/dev/null;; esac; done; true"
}
# The PID, on the node, of a worker pod's container: what `nsenter -n`
# needs to shape its egress (S23). Empty when crictl cannot find it.
# Asked for by name (`.info.pid`), never by grepping the inspect JSON:
# the first `"pid"` in that document is `"pid": 1` in the container's
# process spec, and a grep took it — so the first run of S23 shaped the
# kind NODE's own interface (pid 1's netns), left the pod untouched, and
# throttled everything leaving the node. Verified: the pid this returns
# has a netns other than pid 1's, with the pod's eth0 in it.
# A real node may carry no crictl (AL2023 kubeadm nodes ship `ctr` only,
# the first EC2 run found): then any process in the pod's cgroup but the
# sandbox's pause serves — every one of them is in the pod's netns.
worker_pid() {
    local pid uid u
    pid=$(onnode "p=\$(crictl pods -q --name $1 2>/dev/null | head -1); c=\$(crictl ps -q --pod \$p 2>/dev/null | head -1); [ -n \"\$c\" ] && crictl inspect -o go-template --template '{{.info.pid}}' \$c 2>/dev/null")
    if [ -z "$pid" ]; then
        uid=$($K -n $WNS get pod "$1" -o jsonpath='{.metadata.uid}' 2>/dev/null); u=$(printf '%s' "$uid" | tr -d '-')
        [ -n "$u" ] && pid=$(onnode "for p in /proc/[0-9]*; do [ \"\$(cat \$p/comm 2>/dev/null)\" = pause ] && continue; \
            case \"\$(tr -d '_-' < \$p/cgroup 2>/dev/null)\" in *pod$u*) echo \${p#/proc/}; break;; esac; done")
    fi
    printf '%s' "$pid"
}
# The worker pod serving a tenant pod, by annotation.
# Like `worker_of` but matches a worker in ANY phase. S17 needs it: the
# checkout of a 200-file project finishes in under 10 s (measured on
# this rig — a control arm with no plugin roll reached Running in 10 s),
# so waiting for the worker to be Running spends most of the window
# before the leg has done anything. The worker's tree hostPath is in its
# SPEC, so it is readable the moment the object exists.
worker_of_any() {
    $K -n $WNS get pods -o json 2>/dev/null | python3 -c "
import json,sys
want='$NS/$1'
for p in json.load(sys.stdin)['items']:
    if p['metadata'].get('annotations',{}).get('chert.us/tenant-pod')==want and not p['metadata'].get('deletionTimestamp'):
        print(p['metadata']['name']); break"
}
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
# Keyed on the pod's UID while it exists: events live an hour and are
# looked up by name, so a re-created pod's first query returns its
# predecessor's verdicts (a one-off judged a stale event at 0 s).
mount_events() { local u; u=$($K -n $NS get pod "$1" -o jsonpath='{.metadata.uid}' 2>/dev/null); $K -n $NS get events --field-selector involvedObject.name="$1"${u:+,involvedObject.uid=$u} -o jsonpath='{range .items[*]}{.reason}: {.message}{"\n"}{end}' 2>/dev/null; }
# The fixtures name the MinIO rig's endpoint, bucket and region; STORE=s3
# rewrites them at apply time. On the kind rig every substitution is the
# identity, so the fixtures stay readable as written.
fx() { sed -e "s#http://minio.flint-system.svc:9000#$S3_ENDPOINT#g" -e "s#bucket: s3bucket#bucket: $BUCKET#g" -e "s#region: us-east-1#region: $S3_REGION#g" "$1"; }
apply_fx()  { fx "$1" | $K apply -f -; }
delete_fx() { local f=$1; shift; fx "$f" | $K delete -f - "$@"; }
# The store half of the rig: MinIO and its seed, or the real bucket's
# seed, mc pod and Secrets (rig-s3.yaml.tpl, rendered with the key).
rig() {
    if [ "$STORE" = s3 ]; then
        sed -e "s#__ENDPOINT__#$S3_ENDPOINT#g" -e "s#__BUCKET__#$BUCKET#g" -e "s#__REGION__#$S3_REGION#g" \
            -e "s#__KEY__#$AWS_ACCESS_KEY_ID#g" -e "s#__SECRET__#$AWS_SECRET_ACCESS_KEY#g" rig-s3.yaml.tpl
    else
        cat rig.yaml
    fi
}

# The chart, installed or upgraded with the rig's settings plus whatever
# a leg asks for. ONE list, so a leg's upgrade cannot silently drop a
# setting setup relied on (and never --reuse-values, which would keep a
# previous leg's override alive under the next).
chart_up() {
    helm --kube-context "$CTX" upgrade --install flint-s3-csi "$REPO/flint-s3-csi-chart" -n $SYS \
        --set node.image.tag="$TAG" --set workers.passthroughImage.tag="$TAG" --set workers.leanImage.tag="$TAG" \
        --set node.image.pullPolicy=IfNotPresent \
        --set broker.backend=static --set broker.static.secretRef=s3-broker-static \
        --set node.credsLifetimeSecs="$CREDS_LIFETIME" --set broker.replicas=1 \
        --set node.region="$S3_REGION" \
        --set workers.quota=true \
        --set node.logLevel=debug --set broker.logLevel=debug "$@"
}
plugin_rolled() { $K -n $SYS rollout status ds/flint-s3-csi-node --timeout=240s >/dev/null 2>&1; }
# The plugin logs with ANSI colour (RUST_LOG's default fmt): a field
# renders as `ESC[3mtenant ESC[0m ESC[2m= ESC[0m value`, so a grep for
# `tenant=…` can never match the raw stream. Stripped here, once.
plugin_log() { $K -n $SYS logs "$(plugin_pod)" 2>/dev/null | sed "s/$(printf '\033')\[[0-9;]*m//g"; }
# ONE document of a fixture file, by kind and name, so a leg can recreate
# a single pod (or a renamed copy of it) without re-applying every CR the
# file holds — which would undo a patch the leg just made.
fx_doc() { fx "$1" | python3 -c "
import sys
k,n=sys.argv[1],sys.argv[2]
for d in sys.stdin.read().split('\n---\n'):
    if ('kind: '+k+'\n') in d and ('  name: '+n+'\n') in d: print(d); print('---')
" "$2" "$3"; }
# S27's missing image, by substrate. On kind the nodes pull from nothing,
# so the tag "arrives" by `kind load`; on a real cluster they pull from
# Docker Hub, so it arrives by a push (the docker login rides on the
# drill's DOCKER_HOST) and the premise — the registry does NOT serve it
# yet — is asked of Hub's tag API, never of a local cache. The tag is
# deleted from Hub afterwards with a JWT from the same login, when the
# credential allows it; else the leg says the tag is left.
HUB_REPO=dilipdalton/flint-s3-worker
on_kind() { case "$CTX" in kind-*) return 0;; *) return 1;; esac; }
image_where() { on_kind && echo "kind nodes" || echo "Docker Hub"; }
image_absent() {
    on_kind && return 0
    [ "$(curl -s -o /dev/null -w '%{http_code}' "https://hub.docker.com/v2/repositories/$HUB_REPO/tags/pull-test" 2>/dev/null)" = "404" ]
}
# The node's own containerd cache is the other place the image can come
# from: run 3 on s3a pulled it, run 4 found it cached, the worker came up
# at once and the 45 s deadline never hit (the two log assertions caught
# the vacuity). `ctr images rm <name>` drops ONE reference — unlike
# `crictl rmi`, which removes the image with every tag it carries
# (b859b38d) — and the node is then asked whether the name is gone.
node_image_drop() {
    onnode "command -v ctr >/dev/null 2>&1 && ctr -n k8s.io images rm docker.io/$HUB_REPO:pull-test >/dev/null 2>&1; ! ctr -n k8s.io images ls -q 2>/dev/null | grep -q '$HUB_REPO:pull-test'"
}
image_lands() {
    docker tag "$HUB_REPO:$TAG" "$HUB_REPO:pull-test" >/dev/null 2>&1 || return 1
    if on_kind; then kind load docker-image "$HUB_REPO:pull-test" --name "${CTX#kind-}" >/dev/null 2>&1
    else docker push "$HUB_REPO:pull-test" >/dev/null 2>&1; fi
}
image_unland() {
    on_kind && return 0
    local user secret jwt code
    user=$(printf 'https://index.docker.io/v1/' | docker-credential-osxkeychain get 2>/dev/null | python3 -c 'import json,sys; print(json.load(sys.stdin)["Username"])' 2>/dev/null)
    secret=$(printf 'https://index.docker.io/v1/' | docker-credential-osxkeychain get 2>/dev/null | python3 -c 'import json,sys; print(json.load(sys.stdin)["Secret"])' 2>/dev/null)
    jwt=$(curl -s -H 'Content-Type: application/json' -d "{\"username\":\"$user\",\"password\":\"$secret\"}" https://hub.docker.com/v2/users/login/ 2>/dev/null | python3 -c 'import json,sys; print(json.load(sys.stdin).get("token",""))' 2>/dev/null)
    code=$(curl -s -o /dev/null -w '%{http_code}' -X DELETE -H "Authorization: JWT $jwt" "https://hub.docker.com/v2/repositories/$HUB_REPO/tags/pull-test/" 2>/dev/null)
    case "$code" in 2*) note "the pull-test tag was deleted from Docker Hub";; *) note "the pull-test tag is LEFT on Docker Hub (delete answered '$code'); remove it by hand before the next run";; esac
}
# The mounter's argv inside a passthrough worker (its image has a shell).
worker_argv() { $K -n $WNS exec "$1" -- sh -c 'for p in /proc/[0-9]*; do tr "\0" " " < $p/cmdline 2>/dev/null; echo; done' 2>/dev/null | grep -- 'mount-s3' | head -1; }

# ── lean store helpers (S11-S14, S20; run-legs.sh imports these) ──
# Store helpers, the lean drill's (run-agent.sh): jq, not grep — the
# manifest is nested JSON.
lobj()   { mcx mc cat "m/$BUCKET/$1" 2>/dev/null; }
lcount() { mcx mc ls --recursive "m/$BUCKET/$1" 2>/dev/null | grep -c . ; }
# Resolve the manifest THROUGH the pointer (see `lptr` below).
# `.flint/lean/current` is the mutable object; the entries live in the
# write-once generation it names. These three helpers read
# `.flint/lean/manifest` — the PRE-pointer key — only as a fallback for
# a bucket an older binary wrote, and never first: after migration that
# key holds a refusal doc with no `.entries` at all.
#
# They read the legacy key FIRST until 2026-09-03, which under the
# pointer layout is simply absent. `lments`/`lmseq` answer 0 for a
# missing object, so the two sides of "seq unchanged" and "entry count
# unchanged" agreed by both being zero — a pass earned by reading
# nothing. S14's `n0 > 0` precondition is what caught it.
#
# Three layouts now, tried newest first: a CHUNK LIST (`.chunks`), one
# generation object (`.entries_key`), and the pre-pointer single key.
# A chunked pointer answers `null` for `entries_key`, so a resolver that
# only knew the middle form would read "null" and fail the same silent
# way — which is why each form is tested for POSITIVELY rather than by
# falling through on empty.
lmbody() {
    local c k addrs a body all
    c=$(lobj "$1/.flint/lean/current")
    if [ -n "$c" ]; then
        if printf '%s' "$c" | jq -e 'has("chunks")' >/dev/null 2>&1; then
            addrs=$(printf '%s' "$c" | jq -r '.chunks[].addr')
            all='{"entries":{}}'
            for a in $addrs; do
                body=$(lobj "$1/.flint/lean/chunks/$a")
                # A chunk the pointer names and the bucket does not
                # have is a HOLE, not an empty manifest. Fail — and SAY
                # SO, because the callers map a failed resolve to 0 and
                # an assertion downstream could otherwise pass while
                # reading a short document. S14's `n0 > 0` precondition
                # is the structural guard; this is so the log shows why.
                if [ -z "$body" ]; then
                    echo "  NOTE: pointer for $1 names chunk $a, which the bucket does not have" >&2
                    return 1
                fi
                all=$(printf '%s\n%s' "$all" "$body" \
                        | jq -s '{entries: (.[0].entries + .[1].entries)}')
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
# Objects under the layout's entries prefix: chunks when chunked,
# generations when not. S14 compares this across a takeover, and
# counting the wrong prefix would compare 0 to 0.
lgens()  {
    if lptr "$1" 'has("chunks")' 2>/dev/null | grep -q true; then
        mcx mc ls "m/$BUCKET/$1/.flint/lean/chunks/" 2>/dev/null | grep -c .
    else
        mcx mc ls "m/$BUCKET/$1/.flint/lean/manifests/" 2>/dev/null | grep -c .
    fi
}
# The identity of the ENTRIES a pointer names, whichever layout it is
# on: the sorted chunk address list, or the single generation key.
# Chunks are CONTENT-ADDRESSED, so an unchanged list is proof the bytes
# were not rewritten — a stronger statement than the etag comparison it
# replaces, and one that needs no extra request.
lments_id() {
    local c
    c=$(lobj "$1/.flint/lean/current")
    [ -z "$c" ] && return 1
    printf '%s' "$c" | jq -r 'if has("chunks") then ([.chunks[].addr] | sort | join(",")) else (.entries_key // "") end'
}
# The FENCING seq is the pointer's, not the generation's: a takeover
# rotation bumps the pointer and leaves `entries_seq` alone, which is
# the entire point of the layout.
lmseq()  {
    local c m
    c=$(lobj "$1/.flint/lean/current")
    [ -n "$c" ] && { printf '%s' "$c" | jq -r '.seq // 0'; return; }
    m=$(lobj "$1/.flint/lean/manifest"); [ -z "$m" ] && { echo 0; return; }
    printf '%s' "$m" | jq -r '.seq // 0'
}
# The epoch cell is the lease: holder_id, epoch, released. `lepoch <prefix> <jq>`.
lepoch() { local e; e=$(lobj "$1/.flint/lean/epoch"); [ -z "$e" ] && return 1; printf '%s' "$e" | jq -r "$2"; }
lments() { local m; m=$(lmbody "$1"); [ -z "$m" ] && { echo 0; return; }; printf '%s' "$m" | jq -r '.entries | length'; }
# Kill the syncer INSIDE a worker without touching its pod: the container
# restarts (workers are restartPolicy OnFailure) and relaunches from the
# persisted launch message over the SAME tree — the self-recognition path.
# /proc + kill only; PID 1 cannot be signalled from inside its own namespace.
sig_syncer() { $K -n $WNS exec "$1" -- /bin/sh -c "for p in /proc/[0-9]*; do [ \"\$(cat \$p/comm 2>/dev/null)\" = flint-sync ] && kill -$2 \"\${p#/proc/}\"; done; exit 0" >/dev/null 2>&1; }
kill_syncer() { sig_syncer "$1" 9; }
# The epoch cell's renewed_unix is the liveness signal a successor
# judges. Frozen holder ⇒ it stops advancing, and the cell stays
# `released: false` because nothing ran the release.
lrenew() { lepoch "$1" .renewed_unix; }
# The manifest pointer layout (docs/plans/flint-lean-manifest-pointer-design.md):
# `.flint/lean/current` is the only mutable metadata object; entries live
# in write-once `.flint/lean/manifests/<seq>-<uuid>`.
lptr()   { local c; c=$(lobj "$1/.flint/lean/current"); [ -z "$c" ] && return 1; printf '%s' "$c" | jq -r "$2"; }
lmhas()  { local m; m=$(lmbody "$1"); [ -z "$m" ] && return 1; printf '%s' "$m" | jq -e --arg p "$2" '.entries | has($p)' >/dev/null; }
# The publish fence is held for ONE commit section (lease.rs module doc,
# design 2026-09-13 §4): between barriers nobody holds the cell, so a
# lease verdict cannot be observed by freezing a holder — it is STAGED.
# `lcell_stage` writes the cell the syncer must judge, in the S3
# backend's own shape (crates/flint-store/src/s3.rs EpochBody: holder_id,
# epoch, renewed_unix, salt, released, and the optional handoff); the
# token the syncer CASes on is the object's ETag, and the salt makes every
# stage move it. `lpublish` writes S12's sentinel and waits for an ack
# carrying its nonce, printing "<secs> <ack>": the latency is what the
# quiet polls leave behind. `lincarnation` reads the pod's own id from
# the tenant's view of the tree (`.flint-sync/incarnation.json`).
lcell_stage() { # <prefix> <holder> <epoch> <released:true|false> [handoff]
    local body="{\"holder_id\":\"$2\",\"epoch\":$3,\"renewed_unix\":$(date +%s),\"salt\":\"$(date +%s)-$RANDOM$RANDOM\",\"released\":$4${5:+,\"handoff\":\"$5\"}}"
    mcx sh -c "printf '%s' '$body' | mc pipe m/$BUCKET/$1/.flint/lean/epoch" >/dev/null 2>&1
}
lcell_rm() { mcx mc rm "m/$BUCKET/$1/.flint/lean/epoch" >/dev/null 2>&1; }
lpublish() { # <pod> <nonce> <secs>  → prints "<elapsed> <ack>"; 0 iff the ack carries the nonce
    local p=$1 nonce=$2 budget=$3 i=0 ack="" t0
    t0=$(date +%s)
    tsh "$p" "mkdir -p /workspace/.flint && printf '{\"nonce\":\"$nonce\"}' > /workspace/.flint/publish.tmp && mv /workspace/.flint/publish.tmp /workspace/.flint/publish" \
        || { echo "-1 sentinel-not-written"; return 1; }
    while [ $i -lt "$budget" ]; do
        ack=$(tsh_out "$p" "cat /workspace/.flint/publish.ack 2>/dev/null")
        case "$ack" in *"$nonce"*) break ;; esac
        sleep 2; i=$((i + 2))
    done
    printf '%s %s\n' "$(( $(date +%s) - t0 ))" "$(printf '%s' "$ack" | tr -d '\n')"
    case "$ack" in *"$nonce"*) return 0 ;; *) return 1 ;; esac
}
lincarnation() { tsh_out "$1" "cat /workspace/.flint-sync/incarnation.json 2>/dev/null" | jq -r '.holder_id // empty' 2>/dev/null; }
ack_status() { printf '%s' "$1" | jq -r '.status // "?"' 2>/dev/null; }

# ── setup / teardown ─────────────────────────────────────────────────
if [ "${1:-}" = "setup" ]; then
    set -e
    # DELETE THE SEED JOB FIRST. MinIO's storage is ephemeral, so it
    # loses the bucket whenever its pod is rescheduled — S16's own drain
    # does exactly that — and a Job that already reads Complete is never
    # re-run by `apply`. Setup then "succeeds" against a bucket that does
    # not exist, and the failure surfaces minutes later as a lean publish
    # that cannot write. Recreating the Job every setup is cheap and
    # makes seeding unconditional.
    $K -n $SYS delete job seed-bucket --ignore-not-found --wait=true --timeout=60s >/dev/null 2>&1
    # A teardown's namespaces can still be Terminating: an apply into one
    # is Forbidden, and the waits below then hang on objects that were
    # never created (twice on s3a, 2026-09-30). Wait them out first.
    i=0
    while [ $i -lt 600 ] && { [ "$($K get ns $SYS -o jsonpath='{.status.phase}' 2>/dev/null)" = Terminating ] || [ "$($K get ns $NS -o jsonpath='{.status.phase}' 2>/dev/null)" = Terminating ]; }; do
        sleep 3; i=$((i + 3))
    done
    if [ $i -gt 0 ]; then echo "waited ${i}s for terminating namespaces"; fi
    rig | $K apply -f -
    $K apply -f "$REPO/flint-passthrough-chart/crds/flintpassthroughmounts.yaml"
    $K apply -f "$REPO/flint-lean-chart/crds/flintleanworkspaces.yaml" 2>/dev/null || true
    echo "waiting for the store + seed ($STORE)…"
    [ "$STORE" = s3 ] || $K -n $SYS rollout status deploy/minio --timeout=180s
    $K -n $SYS wait --for=condition=complete job/seed-bucket --timeout=180s
    $K -n $SYS wait --for=condition=ready pod/mc-s3 --timeout=120s
    # And VERIFY it, rather than trusting a Job's condition: the whole
    # point of the delete above is that "Complete" can be stale.
    if ! mcx mc ls m/$BUCKET/ >/dev/null 2>&1; then
        echo "REFUSING: the store has no bucket $BUCKET after seeding — every lean leg would read an empty project" >&2
        exit 1
    fi
    echo "seeded:"; mcx mc ls --recursive m/$BUCKET/
    chart_up
    # workers.quota=true: S18 tests the ceiling, which is off by default
    # since 2026-09-15 (a plain directory is faster; the chart says why).
    $K -n $SYS rollout status ds/flint-s3-csi-node --timeout=180s
    $K -n $SYS rollout status deploy/flint-s3-broker --timeout=180s
    apply_fx tenants.yaml
    apply_fx refusals.yaml
    # One privileged sleeper per node, so `onnode` is an exec and not a
    # pod lifecycle per call (scripts/nodesh-daemon.sh).
    [ "$NODE_EXEC" = nodesh ] && "$REPO/scripts/nodesh-daemon.sh" up
    echo "setup done (store=$STORE node=$NODE via $NODE_EXEC)"
    exit 0
fi
if [ "${1:-}" = "teardown" ]; then
    # 1. THAW. S14 SIGSTOPs a syncer on purpose. A frozen syncer cannot
    #    finish its drain, so its worker never terminates and
    #    NodeUnpublish never completes — and a leg that dies before its
    #    SIGCONT (an `unbound variable` under `set -u` did it once)
    #    leaves the whole rig wedged. Thawing here makes teardown robust
    #    to a leg dying ANYWHERE, not just to that one bug.
    for w in $($K -n $WNS get pods -o name 2>/dev/null); do
        $K -n $WNS exec "${w#pod/}" -- /bin/sh -c \
            'for p in /proc/[0-9]*; do [ "$(cat $p/comm 2>/dev/null)" = flint-sync ] && kill -CONT "${p#/proc/}"; done; exit 0' \
            >/dev/null 2>&1 || true
    done
    delete_fx refusals.yaml --ignore-not-found --wait=false
    # 2. EVERY tenant pod, before the plugin goes. `tenants.yaml` is not
    #    all of them: the lean fixtures (lean-agent, lean-agent2,
    #    slow-agent, the wide ones) hold CSI volumes too, and the helm
    #    uninstall below takes the node plugin with it. Kubelet cannot
    #    NodeUnpublish a volume whose DRIVER IS GONE, so such a pod sits
    #    Terminating forever and holds the namespace with it — measured:
    #    s3-tenants stuck 32 minutes, which then made the next run's
    #    namespace wait time out and race its own setup.
    if ! $K delete pods --all -n $NS --ignore-not-found --wait=true --timeout=300s >/dev/null 2>&1; then
        echo "WARNING: tenant pods still present after 300s; the plugin uninstall below will" >&2
        echo "         strand them (no driver ⇒ no unpublish): $($K -n $NS get pods -o name 2>/dev/null | tr '\n' ' ')" >&2
    fi
    delete_fx tenants.yaml --ignore-not-found --wait=true --timeout=180s
    helm --kube-context "$CTX" uninstall flint-s3-csi -n $SYS || true
    # 3. ORPHANED WORKERS, after the uninstall and not before. Nothing
    #    else collects them: the plugin that creates a worker is the
    #    only thing that deletes one, and it has just gone. A worker
    #    left behind holds a tree the next setup would adopt, and it is
    #    invisible to any "wait for the namespace to disappear" check
    #    because $WNS is PERMANENT — measured: two survived 48 minutes
    #    and stalled the next run's wait until they were reaped by hand.
    #
    #    Only possible after the uninstall. While the chart is installed
    #    the VAP admits DELETE on a worker from the node SA, that node's
    #    kubelet and the kube-system GC and from nobody else, which is
    #    the property S14 relies on; the uninstall takes the policy with
    #    it.
    $K -n $WNS delete pod --all --force --grace-period=0 --ignore-not-found >/dev/null 2>&1 || true
    rig | $K delete -f - --ignore-not-found --wait=false
    [ "$NODE_EXEC" = nodesh ] && "$REPO/scripts/nodesh-daemon.sh" down
    exit 0
fi

echo "s3.csi.chert.us e2e — context $CTX, node $NODE"
# The refusal fixtures are re-applied here, not only in setup: a leg
# that finds its pod absent reports an empty event, never a verdict.
apply_fx refusals.yaml >/dev/null

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
# ORDER MATTERS, and it used to be a race this leg could lose. Deleting
# `reader` AFTER the plugin is parked cannot work: kubelet has no driver
# to call NodeUnpublishVolume on, so the pod sits Terminating, the
# --wait times out into /dev/null, and the `apply` that follows is a
# silent no-op against a terminating object ("Detected changes to
# resource reader which is currently being deleted"). The pod is then
# ABSENT for every later leg, which reports as eight unrelated failures.
# Delete while the plugin can still unmount, park second.
$K -n $NS delete pod reader --ignore-not-found --wait=true --timeout=120s >/dev/null 2>&1
$K -n $NS get pod reader >/dev/null 2>&1 \
    && bad "reader is still present after its delete — the fail-closed control below would be testing a pod that already has its mount" \
    || ok "PRECONDITION: reader is gone while the plugin can still unmount it"
$K -n $SYS patch ds flint-s3-csi-node -p '{"spec":{"template":{"spec":{"nodeSelector":{"chert.us/absent":"true"}}}}}' >/dev/null
i=0; while [ $i -lt 60 ] && [ "$($K -n $SYS get pods -l app.kubernetes.io/name=flint-s3-csi-node -o name 2>/dev/null | grep -c .)" != "0" ]; do sleep 2; i=$((i + 2)); done
apply_fx tenants.yaml >/dev/null
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
    mcx mc stat m/$BUCKET/datasets/imagenet/newfile.txt >/dev/null 2>&1 && bad "newfile.txt reached the bucket from the RO mount" || ok "nothing landed in the bucket from the RO mount"
fi
if require_pod reader; then
    inpod reader "echo written-by-reader > /mnt/s3/from-reader.txt" && ok "RW write accepted" || bad "RW write refused"
    sleep 2
    got=$(mcx mc cat m/$BUCKET/datasets/imagenet/from-reader.txt)
    [ "$got" = "written-by-reader" ] && ok "the object exists in the bucket with the written bytes" || bad "bucket object: '$got'"
    if [ "$got" = "written-by-reader" ]; then
        inpod reader "rm /mnt/s3/from-reader.txt" && ok "unlink accepted" || bad "unlink refused"
        sleep 2
        mcx mc stat m/$BUCKET/datasets/imagenet/from-reader.txt >/dev/null 2>&1 && bad "object still in the bucket after unlink" || ok "unlink removed the object"
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
    kill_worker "$w" >/dev/null 2>&1 || note "kill_worker reported an error for $w; the read below decides"
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
    apply_fx tenants.yaml >/dev/null
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
# ── S11 lean (design §3.5, §5; S11 + S13) ────────────────────────────────

leg S11 "lean: the checkout gate holds for the app AND its init container; a cold pod finds the seeded project; the syncer lives in the worker, not the pod"
$K -n $NS delete pod lean-agent lean-seeder lean-refused --ignore-not-found --wait=true --timeout=180s >/dev/null 2>&1
mcx mc rm --recursive --force m/$BUCKET/tenants/proj/ >/dev/null 2>&1
apply_fx lean-tenants.yaml >/dev/null
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
    apply_fx lean-agent.yaml >/dev/null
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

# S12 (design §10.2). The B1-B25 / C1-C12 protocol suites in lean/e2e
# are NOT re-targeted here: they never used the webhook, they create no
# CR, and they need a subtree per leg — a CSI worker is one volume and
# one prefix, so they stay where they are and keep testing the protocol.
# What CSI delivery changes, and what this leg covers, is the in-band
# publish verb driven from the tenant pod and the exec surface §3.2 had
# to replace: `flint-sync ctl` is unreachable for a tenant now, so the
# control socket inside its own mount of the tree is what it gets.
leg S12 "lean: the in-band publish verb is acked under CSI delivery, the manifest advances, and the control door is reachable in the TENANT's own view of the tree"
if require_pod lean-agent; then
    seq0=$(lmseq tenants/proj)
    # A `test -f` that comes back FALSE and an exec that never ran look
    # identical from here, so prove the channel first: everything below
    # reads a false as an answer.
    tsh lean-agent "test -d /workspace/src" \
        && ok "the exec channel into the tenant works (a FALSE below is an answer, not a broken exec)" \
        || bad "cannot exec into lean-agent at all — every observation this leg makes would be indistinguishable from absence"
    # floorSecs is an hour on this workspace, so the cadence CANNOT
    # advance the manifest: any advance below is the sentinel's doing.
    if tsh lean-agent "test -f /workspace/.flint/publish.ack"; then
        bad "PRECONDITION: a publish.ack existed before this leg asked for one — the round trip would prove nothing"
    else
        ok "PRECONDITION: no publish.ack yet, manifest at seq $seq0"
    fi
    tsh lean-agent "printf 'in-band publish' > /workspace/src/s12.txt" || bad "the tenant could not write into its own workspace"
    lmhas tenants/proj src/s12.txt \
        && bad "PRECONDITION: src/s12.txt is cited before any publish was requested — the floor is not holding and the leg proves nothing" \
        || ok "PRECONDITION: src/s12.txt is written but uncited (the 1-hour floor is the fixture)"
    NONCE="s12-$(date +%s)"
    tsh lean-agent "mkdir -p /workspace/.flint && printf '{\"nonce\":\"$NONCE\"}' > /workspace/.flint/publish.tmp && mv /workspace/.flint/publish.tmp /workspace/.flint/publish" \
        || bad "the tenant could not write the publish sentinel"
    i=0; ack=""
    while [ $i -lt 90 ]; do
        ack=$(tsh_out lean-agent "cat /workspace/.flint/publish.ack 2>/dev/null")
        case "$ack" in *"$NONCE"*) break ;; esac
        sleep 3; i=$((i + 3))
    done
    case "$ack" in
        *"$NONCE"*) ok "the sentinel was acked in ${i}s, carrying THIS leg's nonce (not a stale ack)" ;;
        *) bad "no ack carrying $NONCE within 90 s (ack: $(printf '%s' "$ack" | tr -d '\n' | cut -c1-140))" ;;
    esac
    astat=$(printf '%s' "$ack" | jq -r '.status // "?"' 2>/dev/null)
    [ "$astat" = "ok" ] && ok "the ack status is ok" || bad "the ack status is '$astat'"
    seq1=$(lmseq tenants/proj)
    [ "${seq1:-0}" -gt "${seq0:-0}" ] && ok "the manifest advanced $seq0 → $seq1 (A5)" || bad "the manifest did not advance past seq $seq0"
    lmhas tenants/proj src/s12.txt && ok "the manifest cites src/s12.txt" || bad "the seq advanced but src/s12.txt is not cited"
    [ "$(lobj tenants/proj/files/src/s12.txt)" = "in-band publish" ] && ok "the bucket carries the bytes the tenant wrote" || bad "the object reads '$(lobj tenants/proj/files/src/s12.txt)'"

    # §3.2's replacement for the lost in-pod exec surface.
    tsh lean-agent "test -S /workspace/.flint-sync/ctl.sock" \
        && ok "the control door is a SOCKET in the tenant's own view (/workspace/.flint-sync/ctl.sock)" \
        || bad "no control socket in the tenant's view of the tree (the CR sets udsDoor: true — §3.2's replacement for \`flint-sync ctl\` needs it)"
    w=$(worker_of lean-agent)
    if [ -n "$w" ]; then
        tree=$($K -n $WNS get pod "$w" -o jsonpath='{.spec.volumes[*].hostPath.path}')
        ti=$(tsh_out lean-agent "stat -c %i /workspace/.flint-sync/ctl.sock")
        ni=$(onnode "stat -c %i $tree/.flint-sync/ctl.sock")
        [ -n "$ti" ] && [ "$ti" = "$ni" ] \
            && ok "it is the SAME socket the worker bound (inode $ti) — a tenant that curls --unix-socket reaches the live door" \
            || bad "tenant inode '$ti' vs plugin-tree inode '$ni': the tenant's socket is not the worker's door"
        st=$($K -n $WNS exec "$w" -- /usr/local/bin/flint-sync ctl status 2>&1); rc=$?
        if [ $rc -eq 0 ] && printf '%s' "$st" | jq -e 'type == "object" and (has("error") | not)' >/dev/null 2>&1; then
            ok "the door ANSWERS: flint-sync ctl status returns a status document from the running syncer"
        else
            bad "flint-sync ctl status in $w (rc=$rc): $(printf '%s' "$st" | tr -d '\n' | cut -c1-140)"
        fi
        so=$($K -n $WNS exec "$w" -- /usr/local/bin/flint-sync status 2>&1); rc=$?
        [ $rc -eq 0 ] && printf '%s' "$so" | jq -e . >/dev/null 2>&1 \
            && ok "the operator-side recipe works: flint-sync status runs in $w while the syncer holds the occupancy flock" \
            || bad "flint-sync status in $w (rc=$rc): $(printf '%s' "$so" | tr -d '\n' | cut -c1-140)"
    else
        bad "no Running worker for lean-agent"
    fi
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

# ── S14 holder identity under the per-barrier fence ──────────────────
# The publish fence is held for ONE commit section (lease.rs module doc,
# design 2026-09-13 §4): between barriers nobody holds the cell. The
# 2026-09-03 version of this leg froze the holder and applied a second
# pod — the LIFETIME lease's shape — and under the per-barrier fence it
# froze nothing: the cell it read was the S13 drain's released leftover,
# neither pod's incarnation had ever claimed, and every observation was
# of an inert cell (6 BAD on s3a 2026-09-30; PRECONDITION failures on
# kind since 09-17). A verdict is now STAGED — the cell the syncer must
# judge is written in the store's own shape (`lcell_stage`) — and
# observed through ONE triggered publish (`lpublish`, S12's sentinel),
# whose latency is what the quiet polls leave behind:
#
#   cell staged                          latency          rotation   epoch   holder after
#   RELEASED, reserved for nobody        on sight         none       +1      this pod
#   HELD by a stranger, never renewed    >= 50 s (6 × 10) YES        +1      this pod
#   RELEASED, reserved for a stranger    >= 10 s (2 × 10) none       +1      this pod
#   HELD by THIS pod's previous container  released at the restart, before any claim
#
# The epoch bumps on every claim, so an assertion on the epoch alone
# passes either way; the latency and the manifest seq (+1 for an
# install, +2 for a rotation then an install) tell the verdicts apart.
# A no-change publish would observe nothing: the barrier's early exit
# comes BEFORE the claim (barrier.rs), so every arm publishes one file.
leg S14 "lean holder identity under the per-barrier fence: a released cell is claimed on sight, a HELD stranger is waited out and deposed with a rotation, a reserved handoff is waited out without one, and a fence a previous container of this pod left held is released at its restart"
# An `apply` against a still-terminating object is a silent no-op (the
# S1 lesson), and every observation below would then be made against the
# PREVIOUS pod. Refuse to start rather than measure the wrong thing.
$K -n $NS delete pod lean-agent lean-agent2 --ignore-not-found --wait=true --timeout=240s >/dev/null 2>&1
$K get -n $NS pod lean-agent >/dev/null 2>&1 && bad "PRECONDITION: lean-agent still exists before S14 applies it — the apply would be a no-op against a terminating object"
apply_fx lean-agent.yaml >/dev/null
if wait_phase lean-agent Running 300; then
    inc=$(lincarnation lean-agent)
    e0=$(lepoch tenants/proj .epoch 2>/dev/null); s0=$(lmseq tenants/proj); n0=$(lments tenants/proj)
    [ -n "$inc" ] && [ "${n0:-0}" -gt 0 ] \
        && ok "PRECONDITION: lean-agent's incarnation is $inc; the cell reads epoch ${e0:-none}, the manifest seq $s0 with $n0 entries" \
        || bad "PRECONDITION: no incarnation id in the tenant's view ('$inc') or an empty manifest ($n0 entries) — nothing below could be attributed"
    E=$(( ${e0:-0} + 10 ))

    # ── a RELEASED cell reserved for nobody: a clean handoff ─────────
    lcell_stage tenants/proj lean-stranger-released $E true
    tsh lean-agent "printf 'arm1' > /workspace/src/s14-a.txt"
    out=$(lpublish lean-agent "s14a-$(date +%s)" 120); el=${out%% *}; ack=${out#* }
    [ "$(ack_status "$ack")" = "ok" ] \
        && ok "released cell: the publish was acked ok in ${el}s" \
        || bad "released cell: no ok ack in ${el}s (ack: $(printf '%s' "$ack" | cut -c1-140))"
    [ "${el:-999}" -lt 30 ] \
        && ok "released cell: claimed on sight (${el}s) — a clean handoff is not waited out" \
        || bad "released cell: ${el}s to the ack — a released cell reserved for nobody was waited out"
    h=$(lepoch tenants/proj .holder_id); e=$(lepoch tenants/proj .epoch); rel=$(lepoch tenants/proj .released); s=$(lmseq tenants/proj)
    [ "$h" = "$inc" ] && ok "the cell names THIS pod's incarnation ($h), not the released stranger" || bad "the cell names '$h' after a publish by $inc"
    [ "${e:-0}" = "$((E + 1))" ] && ok "epoch $E → $e: the claim superseded the released cell by one" || bad "epoch $E → '${e:-?}' across a clean handoff"
    [ "$rel" = "true" ] && ok "the cell is released again after the barrier — the fence is held for one commit section" || bad "the cell reads released=$rel after the barrier ended"
    [ "${s:-0}" = "$((s0 + 1))" ] && ok "manifest seq $s0 → $s: one install and no rotation for a clean handoff" || bad "manifest seq $s0 → '${s:-?}': expected exactly +1 (a rotation would double-bump)"
    lmhas tenants/proj src/s14-a.txt && ok "src/s14-a.txt is cited" || bad "src/s14-a.txt is not cited after an ok ack"

    # ── a cell HELD by a stranger that never renews: deposal + rotation ──
    # THE ANTI-VACUITY CONTROL is the latency: a syncer that walked in
    # would show the same new holder and a bumped epoch; only >= 50 s
    # separates "judged dead across six spaced polls" from "superseded on
    # sight".
    E=$((E + 10)); s1=$(lmseq tenants/proj); n1=$(lments tenants/proj)
    lcell_stage tenants/proj lean-straggler-held $E false
    tsh lean-agent "printf 'arm2' > /workspace/src/s14-b.txt"
    out=$(lpublish lean-agent "s14b-$(date +%s)" 200); el=${out%% *}; ack=${out#* }
    [ "$(ack_status "$ack")" = "ok" ] \
        && ok "held cell: the publish was acked ok in ${el}s" \
        || bad "held cell: no ok ack in ${el}s (ack: $(printf '%s' "$ack" | cut -c1-140))"
    [ "${el:-0}" -ge 50 ] && [ "${el:-999}" -le 160 ] \
        && ok "held cell: ${el}s to the ack — six spaced quiet polls (>= 50 s) before the stranger was judged dead, inside the 150 s claim deadline" \
        || bad "held cell: ${el}s to the ack — under 50 s it deposed a holder it never judged dead; over 160 s the barrier hit its deadline"
    w=$(worker_of lean-agent)
    $K -n $WNS logs "$w" 2>/dev/null | grep -q "waiting for the publish fence behind lean-straggler-held" \
        && ok "the syncer reported the wait (behind lean-straggler-held)" \
        || bad "no 'waiting for the publish fence behind lean-straggler-held' line in the worker log"
    h=$(lepoch tenants/proj .holder_id); e=$(lepoch tenants/proj .epoch); s=$(lmseq tenants/proj); n=$(lments tenants/proj)
    [ "$h" = "$inc" ] && ok "the cell names this pod ($h): the stranger was DEPOSED" || bad "the cell names '$h': the stranger was not deposed"
    [ "${e:-0}" = "$((E + 1))" ] && ok "epoch $E → $e across the deposal" || bad "epoch $E → '${e:-?}' across the deposal"
    [ "${s:-0}" = "$((s1 + 2))" ] \
        && ok "manifest seq $s1 → $s: the takeover ROTATED (+1, entries kept) before the install (+1) — a straggler's CAS is stale" \
        || bad "manifest seq $s1 → '${s:-?}': expected +2 (rotation, then install); +1 means no rotation, and a straggler mid-commit could publish over this pod"
    [ "${n:-0}" = "$((n1 + 1))" ] && ok "the entries survived the rotation ($n1 → $n: the one new file)" || bad "entries $n1 → '${n:-?}' across the rotation"

    # ── a RELEASED cell reserved for a stranger: the handoff is waited out ──
    E=$((E + 10)); s2=$(lmseq tenants/proj)
    lcell_stage tenants/proj lean-stranger-released $E true lean-dead-waiter
    tsh lean-agent "printf 'arm3' > /workspace/src/s14-c.txt"
    out=$(lpublish lean-agent "s14c-$(date +%s)" 120); el=${out%% *}; ack=${out#* }
    [ "$(ack_status "$ack")" = "ok" ] \
        && ok "reserved handoff: the publish was acked ok in ${el}s" \
        || bad "reserved handoff: no ok ack in ${el}s (ack: $(printf '%s' "$ack" | cut -c1-140))"
    [ "${el:-0}" -ge 10 ] && [ "${el:-999}" -lt 50 ] \
        && ok "reserved handoff: ${el}s to the ack — two spaced quiet polls for the reserved waiter (>= 10 s), well short of a deposal (50 s)" \
        || bad "reserved handoff: ${el}s to the ack — under 10 s it skipped a reservation it never judged abandoned; 50 s or more is a deposal wait on a released cell"
    h=$(lepoch tenants/proj .holder_id); e=$(lepoch tenants/proj .epoch); s=$(lmseq tenants/proj)
    [ "$h" = "$inc" ] && ok "the cell names this pod ($h): the abandoned reservation was skipped" || bad "the cell names '$h' after the reserved handoff"
    [ "${e:-0}" = "$((E + 1))" ] && ok "epoch $E → $e" || bad "epoch $E → '${e:-?}'"
    [ "${s:-0}" = "$((s2 + 1))" ] && ok "manifest seq $s2 → $s: no rotation for a released cell, whoever it was reserved for" || bad "manifest seq $s2 → '${s:-?}': expected exactly +1"

    # ── a fence THIS pod's previous container left held: released at the restart ──
    # The one case where immediate self-recognition is safe: the
    # incarnation id is emptyDir-scoped, so only a restarted container of
    # the same pod inherits it, and its previous life's process died with
    # it. flint-sync releases such a cell at startup (release_stale_own),
    # before anyone pays the deposal wait for it.
    E=$((E + 10)); s3=$(lmseq tenants/proj)
    lcell_stage tenants/proj "$inc" $E false
    w=$(worker_of lean-agent)
    r0=$($K -n $WNS get pod "$w" -o jsonpath='{.status.containerStatuses[0].restartCount}' 2>/dev/null)
    kill_syncer "$w"
    i=0; while [ $i -lt 90 ]; do
        r1=$($K -n $WNS get pod "$w" -o jsonpath='{.status.containerStatuses[0].restartCount}' 2>/dev/null)
        [ "${r1:-0}" -gt "${r0:-0}" ] && break
        sleep 3; i=$((i + 3))
    done
    [ "${r1:-0}" -gt "${r0:-0}" ] \
        && ok "the syncer died and its container restarted in place (restartCount ${r0:-?} → ${r1:-?} in ${i}s) — the same pod, the same tree" \
        || bad "the worker did not restart after its syncer was killed (restartCount stuck at ${r0:-?}) — the restart arm never ran"
    i=0; while [ $i -lt 90 ] && [ "$(lepoch tenants/proj .released)" != "true" ]; do sleep 3; i=$((i + 3)); done
    h=$(lepoch tenants/proj .holder_id); e=$(lepoch tenants/proj .epoch); rel=$(lepoch tenants/proj .released)
    [ "$rel" = "true" ] && [ "$h" = "$inc" ] && [ "${e:-0}" = "$E" ] \
        && ok "the restarted container RELEASED the fence its previous life left held (holder $inc, epoch $E, released within ${i}s) — no waiter pays the deposal for it" \
        || bad "after the restart the cell reads holder=$h epoch=${e:-?} released=$rel — a fence left held by this pod's own previous container was not released"
    $K -n $WNS logs "$w" 2>/dev/null | grep -q "the publish fence was left held by a previous container of this pod" \
        && ok "the worker log names the repair" \
        || bad "no 'left held by a previous container' line in the restarted worker's log"
    inc2=$(lincarnation lean-agent)
    [ -n "$inc2" ] && [ "$inc2" = "$inc" ] \
        && ok "the incarnation id survived the restart ($inc2) — it lives in the tree, not in the container" \
        || bad "the incarnation id changed across a mere container restart ($inc → '$inc2')"
    [ "$(lmseq tenants/proj)" = "$s3" ] && ok "the restart moved no manifest (seq still $s3) — rotation is for stragglers, not restarts" || bad "the restart moved the manifest ($s3 → $(lmseq tenants/proj))"
    tsh lean-agent "printf 'arm4' > /workspace/src/s14-d.txt"
    out=$(lpublish lean-agent "s14d-$(date +%s)" 120); el=${out%% *}; ack=${out#* }
    h=$(lepoch tenants/proj .holder_id); e=$(lepoch tenants/proj .epoch); s=$(lmseq tenants/proj)
    [ "$(ack_status "$ack")" = "ok" ] && [ "${el:-999}" -lt 30 ] && [ "$h" = "$inc" ] && [ "${e:-0}" = "$((E + 1))" ] && [ "${s:-0}" = "$((s3 + 1))" ] \
        && ok "the restarted container publishes again under the same incarnation on sight (${el}s; epoch $E → $e, seq $s3 → $s, no rotation)" \
        || bad "after the restart: ack ${el}s status $(ack_status "$ack"), holder '$h', epoch '${e:-?}' (want $((E + 1))), seq '${s:-?}' (want $((s3 + 1)))"
else
    bad "lean-agent never reached Running in 300s — S14 made no observation at all"
fi
$K -n $NS delete pod lean-agent --ignore-not-found --wait=true --timeout=240s >/dev/null 2>&1

# ── S20 an exited syncer relaunched; a drain against a LIVE fence ─────
# Audit 2026-09-03, findings 2 + 3, restated for the per-barrier fence.
# A fence no longer exits the process (barrier.rs: a fenced barrier is
# abandoned and the next floor claims again), so the Succeeded worker
# finding 2 is about is staged the way it happens: the syncer exits 0 on
# its own (SIGTERM) while the tenant is still mounted. OnFailure ignores
# exit 0; the plugin must relaunch it on the next republish. Finding 3:
# at the tenant's delete the drain meets a fence held by a LIVE stranger
# — renewed from the rig every 5 s, so its token never stands still and
# no quiet poll ever judges it dead — and cannot publish. It exits
# attesting nothing, and the tree must be PRESERVED. The workspace's
# floor is lowered to 60 s for this pod only: the derived grace (floor +
# 81 s) is also the drain's retry budget, and at the fixture's one-hour
# floor the delete would stand for an hour.
leg S20 "a syncer that exited on its own is relaunched under its still-mounted tenant; at the tenant's delete a drain that cannot win a LIVE fence attests nothing and its tree is preserved"
$K -n $NS delete pod lean-agent --ignore-not-found --wait=true --timeout=240s >/dev/null 2>&1
$K -n $NS patch flintleanworkspace proj --type merge -p '{"spec":{"floorSecs":60}}' >/dev/null 2>&1
apply_fx lean-agent.yaml >/dev/null
if wait_phase lean-agent Running 300; then
    w=$(worker_of lean-agent); wuid0=$($K -n $WNS get pod "$w" -o jsonpath='{.metadata.uid}' 2>/dev/null)
    grace=$($K -n $WNS get pod "$w" -o jsonpath='{.spec.terminationGracePeriodSeconds}' 2>/dev/null)
    [ -n "$w" ] && ok "PRECONDITION: lean-agent's worker is $w (grace ${grace:-?}s from the 60 s floor)" || bad "PRECONDITION: no worker for lean-agent"
    sig_syncer "$w" TERM
    i=0; ph=""
    while [ $i -lt 90 ]; do
        ph=$($K -n $WNS get pod "$w" -o jsonpath='{.status.phase}' 2>/dev/null)
        [ "$ph" = "Succeeded" ] && break
        u=$($K -n $WNS get pod "$w" -o jsonpath='{.metadata.uid}' 2>/dev/null)
        [ -n "$u" ] && [ "$u" != "$wuid0" ] && { ph="Relaunched"; break; }
        sleep 3; i=$((i + 3))
    done
    xc=$($K -n $WNS get pod "$w" -o jsonpath='{.status.containerStatuses[0].state.terminated.exitCode}' 2>/dev/null)
    if [ "$ph" = "Succeeded" ] && [ "${xc:-1}" = "0" ]; then
        ok "the syncer exited on SIGTERM and its worker is Succeeded (exit 0, ${i}s) while the tenant is still mounted — the shape OnFailure never restarts"
    elif [ "$ph" = "Relaunched" ]; then
        ok "the syncer exited and the plugin had already relaunched its worker (uid $wuid0 replaced, ${i}s)"
    else
        bad "after SIGTERM the worker is '${ph:-?}' exit '${xc:-?}' at ${i}s, not Succeeded/0 — the arm has no exited syncer to relaunch"
    fi
    seen=$(tsh_out lean-agent "ls /workspace/src 2>/dev/null | wc -l")
    [ "${seen:-0}" -gt 0 ] && ok "the tenant still sees its tree ($seen files) with no syncer behind it" || bad "the tenant's tree reads empty ('$seen') once the syncer exited"
    i=0; wuid_r=""
    while [ $i -lt 240 ]; do
        rph=$($K -n $WNS get pod "$w" -o jsonpath='{.status.phase}' 2>/dev/null)
        u=$($K -n $WNS get pod "$w" -o jsonpath='{.metadata.uid}' 2>/dev/null)
        [ "$rph" = "Running" ] && [ -n "$u" ] && [ "$u" != "$wuid0" ] && { wuid_r=$u; break; }
        sleep 5; i=$((i + 5))
    done
    [ -n "$wuid_r" ] \
        && ok "the exited worker was RELAUNCHED by a republish (${i}s; uid $wuid0 → $wuid_r)" \
        || bad "the exited worker was not relaunched in ${i}s (phase '${rph:-?}'): a syncer that exits 0 stays gone for the tenant's life"
    mount_events lean-agent | grep -q 'SyncerRecreated' \
        && ok "the tenant's events name the relaunch (SyncerRecreated)" \
        || bad "no SyncerRecreated event on lean-agent"
    tsh lean-agent "printf 'after the relaunch' > /workspace/src/s20-a.txt"
    out=$(lpublish lean-agent "s20a-$(date +%s)" 120); el=${out%% *}; ack=${out#* }
    [ "$(ack_status "$ack")" = "ok" ] && lmhas tenants/proj src/s20-a.txt \
        && ok "the relaunched syncer publishes (acked in ${el}s, src/s20-a.txt cited)" \
        || bad "the relaunched syncer did not publish: ack ${el}s '$(printf '%s' "$ack" | cut -c1-100)'"

    # ── the delete, against a LIVE fence ─────────────────────────────
    tsh lean-agent "printf 'written after the last publish' > /workspace/src/s20-late.txt"
    E=$(( $(lepoch tenants/proj .epoch 2>/dev/null || echo 0) + 10 ))
    ( i=0; while [ $i -lt 90 ]; do lcell_stage tenants/proj lean-live-stranger $E false; sleep 5; i=$((i + 1)); done ) >/dev/null 2>&1 &
    renew_pid=$!
    sleep 8
    before_und=$(onnode "ls -d /var/lib/kubelet/plugins/s3.csi.chert.us/undrained/* 2>/dev/null | wc -l")
    t0=$(date +%s)
    $K -n $NS delete pod lean-agent --wait=true --timeout=600s >/dev/null 2>&1
    el=$(( $(date +%s) - t0 ))
    kill "$renew_pid" 2>/dev/null; wait "$renew_pid" 2>/dev/null
    # THE ANTI-VACUITY CONTROL: a drain that never met the fence returns
    # in seconds, and what follows would be a different failure's tree.
    [ "$el" -ge 60 ] \
        && ok "the delete stood ${el}s: the drain waited on the live fence rather than walking past it" \
        || bad "the delete returned in ${el}s — the drain never waited on the live fence, so what follows was not earned"
    h=$(lepoch tenants/proj .holder_id)
    [ "$h" = "lean-live-stranger" ] && ok "the cell still names the live stranger ($h): a holder whose token moves is never deposed" || bad "the cell names '$h': the drain deposed a LIVE holder"
    after_und=$(onnode "ls -d /var/lib/kubelet/plugins/s3.csi.chert.us/undrained/* 2>/dev/null | wc -l")
    [ "${after_und:-0}" -gt "${before_und:-0}" ] \
        && ok "the tree was PRESERVED under undrained/ at its tenant's delete (its drain attested nothing)" \
        || bad "no preserved tree appeared under undrained/ ($before_und → $after_und): an unattested drain lost its tree with the pod"
    und=$(onnode "ls -dt /var/lib/kubelet/plugins/s3.csi.chert.us/undrained/* 2>/dev/null | head -1")
    [ -n "$und" ] && onnode "test -d '$und/tree' && test -f '$und/state.json'" \
        && ok "the preserved dir carries the tree and its state ($und)" \
        || bad "the preserved dir is incomplete or absent: $(onnode "ls '${und:-/nonexistent}' 2>&1" | tr '\n' ' ')"
    # With workers.quota=true the tree is an ext4 image (tree.img) and
    # preservation unmounts it, so tree/ is an empty mount point: read
    # through a loop mount of the image, as the event text tells an
    # operator to (1 BAD on s3a 2026-09-30 for reading the directory).
    if onnode "test -f '${und:-/nonexistent}/tree.img'"; then
        shape="tree.img, loop-mounted"
        late=$(onnode "mkdir -p /mnt/s20-und && mount -o loop,ro '${und:-/nonexistent}/tree.img' /mnt/s20-und && cat /mnt/s20-und/src/s20-late.txt; umount /mnt/s20-und 2>/dev/null; rmdir /mnt/s20-und 2>/dev/null")
    else
        shape="tree/"
        late=$(onnode "cat '${und:-/nonexistent}/tree/src/s20-late.txt' 2>/dev/null")
    fi
    [ "$late" = "written after the last publish" ] \
        && ok "the uncited file is in the preserved tree ($shape) — what the drain could not publish was not lost" \
        || bad "src/s20-late.txt is not in the preserved tree ($shape): read '$late'"
    onnode "test -f '${und:-/nonexistent}/tree/.flint-sync/drained.json'" \
        && bad "a drain attestation exists in a tree the plugin preserved — the sensor contradicts the decision" \
        || ok "no drain attestation in the preserved tree — the decision matches its sensor"
    ev=$(mount_events lean-agent)
    echo "$ev" | grep -qE 'DrainNotAttested|DrainCeilingHit' \
        && ok "the tenant's events say why ($(echo "$ev" | grep -oE 'DrainNotAttested|DrainCeilingHit' | sort -u | tr '\n' ' '))" \
        || bad "no DrainNotAttested/DrainCeilingHit event on lean-agent: $(echo "$ev" | tail -2 | cut -c1-200)"
    onnode "rm -rf /var/lib/kubelet/plugins/s3.csi.chert.us/undrained" >/dev/null 2>&1
else
    bad "lean-agent never reached Running in 300s — S20 made no observation at all"
fi
# Leave the workspace as the fixture has it: the hour-long floor, and a
# released cell so the next claim is a clean handoff.
$K -n $NS patch flintleanworkspace proj --type merge -p '{"spec":{"floorSecs":3600}}' >/dev/null 2>&1
lcell_stage tenants/proj lean-live-stranger "$(( $(lepoch tenants/proj .epoch 2>/dev/null || echo 0) + 1 ))" true

# ── S17 a plugin restart mid-checkout ────────────────────────────────
# The syncer is a SEPARATE POD, so rolling the node plugin must not
# restart a checkout in flight. That is the whole reason the M1
# child-process variant was rejected: a plugin that forks the syncer
# takes every checkout on the node down with it on every roll.
#
# The leg is only worth anything if the restart lands while the checkout
# is actually running, so it asserts that as a precondition rather than
# hoping. `fanout: 1` on the fixture is what makes the window wide
# enough to hit.
leg S17 "a node-plugin restart mid-checkout does not restart the checkout: the syncer is a separate pod, and its marker predates the new plugin"
$K -n $NS delete pod slow-agent --ignore-not-found --wait=true --timeout=240s >/dev/null 2>&1
apply_fx lean-slow.yaml >/dev/null
# Wait for the worker to exist — the checkout is running once it does.
# Poll fast and accept ANY phase: every second spent here is a second
# of the window spent. `sleep 2` on a Running-only match routinely
# missed a checkout that takes under ten seconds.
w=""; i=0
while [ $i -lt 480 ] && [ -z "$w" ]; do w=$(worker_of_any slow-agent); [ -z "$w" ] && { sleep 0.25; i=$((i + 1)); }; done
if [ -n "$w" ]; then
    wuid0=$($K -n $WNS get pod "$w" -o jsonpath='{.metadata.uid}' 2>/dev/null)
    # THIS worker's tree, from its own hostPath — never
    # `ls .../volumes/*/tree | head -1`. By S17 several workspaces are
    # mounted on this node, and the first one lexically belongs to
    # whichever volume id sorts first. Reading a NEIGHBOUR's tree made
    # the precondition report a marker that was never slow-agent's, and
    # made the closing mtime assertion compare the new plugin against a
    # completed checkout it had nothing to do with — a pass earned from
    # the wrong file.
    tree=$($K -n $WNS get pod "$w" -o jsonpath='{.spec.volumes[*].hostPath.path}' 2>/dev/null)
    case "$tree" in
        /var/lib/kubelet/plugins/s3.csi.chert.us/volumes/*/tree) ;;
        *) bad "could not resolve slow-agent's own tree from its worker (got '$tree'); every observation below would be about some other workspace"; tree="" ;;
    esac
    marker_present=$(onnode "test -f '$tree/.flint-sync/checkout-complete' && echo yes || echo no")
    [ "$marker_present" = "no" ] \
        && ok "PRECONDITION: the checkout is still running when the plugin is rolled (no marker yet under $tree)" \
        || bad "PRECONDITION: the checkout had already finished before the roll — this leg would prove only that a completed checkout survives, which is not the claim"
    # Roll the plugin.
    p0=$(plugin_pod)
    $K -n $SYS delete pod "$p0" --wait=true --timeout=180s >/dev/null 2>&1
    $K -n $SYS rollout status ds/flint-s3-csi-node --timeout=180s >/dev/null 2>&1
    p1=$(plugin_pod)
    started=$($K -n $SYS get pod "$p1" -o jsonpath='{.status.startTime}' 2>/dev/null)
    [ -n "$p1" ] && [ "$p1" != "$p0" ] \
        && ok "the plugin was rolled mid-checkout ($p0 → $p1)" \
        || bad "the plugin did not roll ($p0 → ${p1:-absent}); nothing was interrupted"
    # THE ANTI-VACUITY ARM. The roll proves nothing unless the NEW plugin
    # found the checkout still running when it adopted the node's
    # volumes. A checkout that finished in the seconds between the
    # precondition above and the new plugin's start is adopted as
    # `published`, and every assertion below passes without the path
    # under test ever being taken — which is how this leg went green
    # over an adoption that CLEANED UP a checkout in progress. The
    # plugin logs which branch adoption took; ask it.
    adopt_log=$($K -n $SYS logs "$p1" -c node 2>/dev/null || true)
    if printf '%s' "$adopt_log" | grep -q "checkout in progress at startup"; then
        ok "the NEW plugin adopted the volume mid-checkout (logged 'checkout in progress at startup'): the path under test was taken"
    elif printf '%s' "$adopt_log" | grep -q "unfinished publish found at startup"; then
        bad "the new plugin CLEANED UP the checkout at startup ('unfinished publish found at startup'): the syncer and its tree were destroyed and the checkout restarted"
    else
        # A fast cluster (3 × i4i.large + an in-cluster store, 2026-09-30)
        # closes the window before the roll lands. That is not the product
        # failing — S17f holds the syncer still and pins the same property
        # deterministically — so the run says INCONCLUSIVE rather than BAD.
        note "INCONCLUSIVE: the new plugin logged neither adoption branch — the checkout had finished before it started, so this run tests nothing (S17f is the deterministic form; widen the window here with more files or fanout 1)"
    fi
    if wait_phase slow-agent Running 400; then
        ok "the tenant reached Running: the checkout COMPLETED across a plugin restart"
        seen=$(inpod slow-agent "cat /tmp/seen")
        [ "${seen:-0}" -ge 200 ] \
            && ok "it found all $seen seeded files — the checkout finished, it did not merely give up" \
            || bad "the tenant sees ${seen:-?} files, not the 200 the project holds"
        wuid1=$($K -n $WNS get pod "$w" -o jsonpath='{.metadata.uid}' 2>/dev/null)
        [ -n "$wuid1" ] && [ "$wuid1" = "$wuid0" ] \
            && ok "the syncer pod is the SAME object across the plugin restart (uid unchanged) — a child-process design would have killed it" \
            || bad "the syncer pod changed across the plugin restart ('$wuid0' → '${wuid1:-gone}'): the checkout was restarted, which is the M1 failure this design rejected"
        # THE ATTRIBUTION. If the new plugin had re-driven the checkout,
        # the marker would be younger than the plugin that wrote it.
        mtime=$(onnode "stat -c %Y '$tree/.flint-sync/checkout-complete' 2>/dev/null")
        pstart=$(date -u -j -f "%Y-%m-%dT%H:%M:%SZ" "$started" +%s 2>/dev/null || echo "")
        if [ -n "$mtime" ] && [ -n "$pstart" ]; then
            [ "$mtime" -lt "$pstart" ] \
                && ok "the checkout marker predates the new plugin ($(date -u -r "$mtime" +%H:%M:%S) < $(date -u -r "$pstart" +%H:%M:%S)) — the new plugin did not redo the work" \
                || bad "the marker is younger than the new plugin ($mtime >= $pstart): the checkout was re-driven after the roll"
        else
            note "could not compare marker mtime ($mtime) with the plugin start ($started)"
        fi
    else
        bad "slow-agent never reached Running in 400s after the plugin was rolled mid-checkout"
    fi
else
    bad "no worker appeared for slow-agent in 120s — S17 made no observation at all"
fi
$K -n $NS delete pod slow-agent --ignore-not-found --wait=true --timeout=240s >/dev/null 2>&1

# ── S17f: the window S17 cannot open, made by hand ───────────────────
# A wide project of its own (never `proj`, whose entry count S11/S13/S14
# assert), seeded through a lean seeder and its drain. 4000 files at
# fanout 1 still check out in ~15 s against local MinIO — less than a
# plugin roll — so the leg below does not rely on timing at all.
leg S17f-seed "seed a 4000-file project (proj-wide) through a lean seeder and its drain"
$K -n $NS delete pod slow-agent wide-seeder --ignore-not-found --wait=true --timeout=240s >/dev/null 2>&1
apply_fx lean-wide.yaml >/dev/null
if wait_phase wide-seeder Succeeded 300; then
    ok "the seeder wrote its files and exited"
    $K -n $NS delete pod wide-seeder --wait=true --timeout=300s >/dev/null 2>&1
    i=0; while [ $i -lt 300 ] && [ "$(lments tenants/proj-wide)" -lt 4000 ]; do sleep 3; i=$((i + 3)); done
    n=$(lments tenants/proj-wide)
    [ "${n:-0}" -ge 4000 ] && ok "the drain published the wide project: $n entries cited (${i}s)" || bad "the wide project has only ${n:-0} entries cited after ${i}s"
else
    bad "the wide seeder did not reach Succeeded"
fi
# ── S17f: adoption keeps a FROZEN checkout ─────────────────────────────
# The timing form of S17 cannot open its window on this rig: a plugin
# roll takes ~30 s and even a 4000-file fanout-1 checkout finishes in
# ~15 s against local MinIO. So the window is made by hand: the syncer
# is SIGSTOPped mid-checkout, the plugin is rolled while the volume
# state reads `checking-out`, and the new plugin's adoption is observed
# directly — its log line, the worker's uid and deletionTimestamp, the
# tree — before the syncer is thawed and the checkout completes.
leg S17f "adoption keeps a checkout in progress: syncer frozen mid-checkout, plugin rolled, volume kept, checkout completes after the thaw"
$K -n $NS delete pod slow-agent --ignore-not-found --wait=true --timeout=240s >/dev/null 2>&1
apply_fx lean-wide-agent.yaml >/dev/null
w=""; i=0
while [ $i -lt 480 ] && [ -z "$w" ]; do w=$(worker_of_any slow-agent); [ -z "$w" ] && { sleep 0.25; i=$((i + 1)); }; done
if [ -n "$w" ]; then
    wuid0=$($K -n $WNS get pod "$w" -o jsonpath='{.metadata.uid}' 2>/dev/null)
    tree=$($K -n $WNS get pod "$w" -o jsonpath='{.spec.volumes[*].hostPath.path}' 2>/dev/null)
    voldir=$(dirname "$tree")
    # Freeze the syncer the moment it exists (the worker's PID 1 spawns
    # it a few seconds after the pod appears).
    frozen=""; i=0
    while [ $i -lt 240 ] && [ -z "$frozen" ]; do
        frozen=$($K -n $WNS exec "$w" -- /bin/sh -c 'for p in /proc/[0-9]*; do [ "$(cat $p/comm 2>/dev/null)" = flint-sync ] && { kill -STOP "${p#/proc/}" && echo "${p#/proc/}"; }; done; exit 0' 2>/dev/null)
        [ -z "$frozen" ] && { sleep 0.5; i=$((i + 1)); }
    done
    [ -n "$frozen" ] && ok "the syncer (pid $frozen in $w) is FROZEN" || bad "could not freeze the syncer in $w"
    marker_present=$(onnode "test -f '$tree/.flint-sync/checkout-complete' && echo yes || echo no")
    [ "$marker_present" = "no" ] \
        && ok "PRECONDITION: no checkout marker while frozen — the checkout is genuinely in progress" \
        || bad "PRECONDITION: the marker already exists under $tree — frozen too late; this run proves nothing"
    st_phase=$(onnode "cat '$voldir/state.json'" | jq -r '.phase' 2>/dev/null)
    [ "$st_phase" = "checking-out" ] \
        && ok "PRECONDITION: the volume state on disk reads 'checking-out' — the phase adoption will see" \
        || bad "PRECONDITION: the volume state reads '${st_phase:-?}', not checking-out"
    p0=$(plugin_pod)
    $K -n $SYS delete pod "$p0" --wait=true --timeout=180s >/dev/null 2>&1
    $K -n $SYS rollout status ds/flint-s3-csi-node --timeout=180s >/dev/null 2>&1
    p1=$(plugin_pod)
    started=$($K -n $SYS get pod "$p1" -o jsonpath='{.status.startTime}' 2>/dev/null)
    [ -n "$p1" ] && [ "$p1" != "$p0" ] \
        && ok "the plugin was rolled while the checkout was frozen ($p0 → $p1)" \
        || bad "the plugin did not roll ($p0 → ${p1:-absent})"
    sleep 3
    adopt_log=$($K -n $SYS logs "$p1" -c node 2>/dev/null || true)
    if printf '%s' "$adopt_log" | grep -q "checkout in progress at startup"; then
        ok "the NEW plugin took the keep-checking-out branch ('checkout in progress at startup')"
    elif printf '%s' "$adopt_log" | grep -q "unfinished publish found at startup"; then
        bad "the new plugin CLEANED UP the checkout at startup — the pre-fix behaviour"
    else
        bad "the new plugin logged neither adoption branch"
    fi
    wuid1=$($K -n $WNS get pod "$w" -o jsonpath='{.metadata.uid}' 2>/dev/null)
    dts=$($K -n $WNS get pod "$w" -o jsonpath='{.metadata.deletionTimestamp}' 2>/dev/null)
    [ -n "$wuid1" ] && [ "$wuid1" = "$wuid0" ] && [ -z "$dts" ] \
        && ok "the frozen syncer's pod survived adoption untouched (uid unchanged, no deletionTimestamp)" \
        || bad "the worker did not survive adoption (uid '$wuid0' → '${wuid1:-gone}', deletionTimestamp '${dts:-none}')"
    tree_there=$(onnode "test -d '$tree' && test -f '$voldir/state.json' && echo yes || echo no")
    [ "$tree_there" = "yes" ] && ok "the tree and the volume state are still on the node" || bad "the tree or its state is gone from the node"
    still=$(onnode "cat '$voldir/state.json'" | jq -r '.phase' 2>/dev/null)
    [ "$still" = "checking-out" ] && ok "the state still reads checking-out after adoption (nothing was rewritten)" || bad "the state now reads '${still:-?}'"
    # Thaw, and the checkout must finish under the NEW plugin.
    $K -n $WNS exec "$w" -- /bin/sh -c 'for p in /proc/[0-9]*; do [ "$(cat $p/comm 2>/dev/null)" = flint-sync ] && kill -CONT "${p#/proc/}"; done; exit 0' >/dev/null 2>&1
    if wait_phase slow-agent Running 400; then
        ok "the tenant reached Running after the thaw: the checkout COMPLETED under the new plugin"
        seen=$(inpod slow-agent "cat /tmp/seen")
        [ "${seen:-0}" -ge 4000 ] \
            && ok "it found all $seen seeded files" \
            || bad "the tenant sees ${seen:-?} files, not the 4000 the project holds"
        wuid2=$($K -n $WNS get pod "$w" -o jsonpath='{.metadata.uid}' 2>/dev/null)
        [ -n "$wuid2" ] && [ "$wuid2" = "$wuid0" ] \
            && ok "the syncer pod is the SAME object end to end (uid unchanged)" \
            || bad "the syncer pod changed ('$wuid0' → '${wuid2:-gone}')"
        mtime=$(onnode "stat -c %Y '$tree/.flint-sync/checkout-complete' 2>/dev/null")
        pstart=$(date -u -j -f "%Y-%m-%dT%H:%M:%SZ" "$started" +%s 2>/dev/null || echo "")
        if [ -n "$mtime" ] && [ -n "$pstart" ]; then
            [ "$mtime" -ge "$pstart" ] \
                && ok "the marker is YOUNGER than the new plugin ($(date -u -r "$mtime" +%H:%M:%S) >= $(date -u -r "$pstart" +%H:%M:%S)): the checkout finished under it, and it was the SAME syncer that finished it" \
                || bad "the marker predates the new plugin: the checkout was not actually in progress across the roll"
        else
            note "could not compare marker mtime ($mtime) with the plugin start ($started)"
        fi
    else
        bad "slow-agent never reached Running in 400s after the thaw"
    fi
else
    bad "no worker appeared for slow-agent in 120s — S17f made no observation at all"
fi
# The wide project leaves the rig before S18.
$K -n $NS delete pod slow-agent --ignore-not-found --wait=true --timeout=240s >/dev/null 2>&1
delete_fx lean-wide.yaml --ignore-not-found --wait=false >/dev/null 2>&1
mcx mc rm --recursive --force m/$BUCKET/tenants/proj-wide >/dev/null 2>&1 || true

# ── S18 the tree's ceiling ───────────────────────────────────────────
# `sizeLimitGib` was an emptyDir sizeLimit under the webhooks, where
# kubelet enforced it. Under CSI the tree is a plugin-owned DIRECTORY on
# the node's root filesystem, so until the loop-image quota shipped the
# field described nothing and one runaway agent could fill the disk the
# kubelet runs on. The ceiling is now a sparse ext4 image mounted at the
# tree, and an overrun is ENOSPC in the tenant's own write.
#
# ENOSPC on its own proves NOTHING: a node whose root disk is full says
# exactly the same thing to exactly the same write. The unbounded
# workspace beside it is the control — if the node were what ran out,
# its df would show no space either.
leg S18 "lean quota: sizeLimitGib is a filesystem, so a workspace fills at its ceiling instead of filling the node — and an unbounded sibling proves the node did not"
apply_fx quota-tenants.yaml >/dev/null
if wait_phase quota-agent Running 300 && wait_phase noquota-agent Running 300; then
    qk=$(inpod quota-agent "df -P /workspace | tail -1 | awk '{print \$2}'")
    nk=$(inpod noquota-agent "df -P /workspace | tail -1 | awk '{print \$2}'")
    [ -n "$qk" ] && [ "$qk" -gt 700000 ] && [ "$qk" -lt 1200000 ] \
        && ok "the quota'd tree is its own filesystem of ${qk}K — sizeLimitGib 1 is a real 1 GiB, not a label" \
        || bad "the quota'd tree reports ${qk:-?}K, not about 1 GiB: the ceiling is not the filesystem the tenant is writing to"
    [ -n "$nk" ] && [ "$nk" -gt "$((qk * 2))" ] \
        && ok "CONTROL: the sizeLimitGib:0 sibling sees the NODE's filesystem (${nk}K) — the opt-out still yields a plain directory" \
        || bad "CONTROL: the unbounded workspace reports ${nk:-?}K, not the node's filesystem: the control cannot distinguish a full ceiling from a full node"
    # THE WORKSPACE IS THE FILESYSTEM ROOT, and mkfs leaves a root-owned
    # lost+found there. The syncer walks the tree as the app's uid, so a
    # directory it cannot enter is EACCES on every barrier and nothing is
    # ever published — invisible to a leg that only writes INTO the
    # workspace, which is how the first cut of this leg missed it.
    lf=$(inpod quota-agent "ls -a /workspace | grep -c '^lost+found$' || true")
    [ "${lf:-0}" = "0" ] \
        && ok "the tenant's workspace has no root-owned lost+found — the tree is walkable by the uid that owns it" \
        || bad "lost+found is present in the workspace: the syncer walks this tree as the app's uid and every barrier will die EACCES"
    walk=$(tsh_out quota-agent "find /workspace -type d >/dev/null 2>&1 && echo WALKED || echo EACCES")
    [ "$walk" = "WALKED" ] \
        && ok "the whole workspace tree is walkable by the tenant's own uid" \
        || bad "the tenant cannot walk its own workspace ('$walk') — the syncer runs as the same uid and will fail the same way"
    loop=$(onnode "grep ' /var/lib/kubelet/plugins/s3.csi.chert.us/volumes/[^ ]*/tree ' /proc/mounts | grep -c '^/dev/loop'")
    [ "${loop:-0}" -ge 1 ] \
        && ok "the ceiling is a loop-mounted image on the node ($loop tree mount(s) on /dev/loop)" \
        || bad "no lean tree is a loop mount on the node — the tree is a plain directory and sizeLimitGib is enforced by nothing"

    free0=$(onnode "df -P / | tail -1 | awk '{print \$4}'")
    # Fill past the ceiling. busybox dd reports the refusal on stderr and
    # stops; what must be true is that it stopped THERE and not at the
    # node's disk.
    # Keep ALL of dd's output: busybox prints the refusal FIRST and then
    # its records/throughput summary, so a `tail -2` reads the summary
    # and throws the verdict away — which is what the first run did.
    out=$(tsh_out quota-agent "dd if=/dev/zero of=/workspace/fill bs=1M count=1500 2>&1 | tr '\n' ' '")
    case "$out" in
        *"No space left"*|*ENOSPC*) ok "the tenant's own write hit the ceiling: $(printf '%s' "$out" | tr '\n' ' ' | cut -c1-90)" ;;
        *) bad "1500 MiB went into a 1 GiB workspace without ENOSPC — the ceiling did not hold: '$(printf '%s' "$out" | tr '\n' ' ' | cut -c1-90)'" ;;
    esac
    wrote=$(inpod quota-agent "wc -c < /workspace/fill 2>/dev/null || echo 0")
    [ -n "$wrote" ] && [ "$wrote" -lt 1288490188 ] \
        && ok "it stopped at $((wrote / 1048576)) MiB — inside the 1 GiB it declared, not somewhere past it" \
        || bad "the workspace holds ${wrote:-?} bytes, past its 1 GiB ceiling"
    # THE CONTROL THAT MATTERS. If the NODE had run out, this write would
    # fail too — and the whole leg would be measuring a full disk.
    tsh noquota-agent "dd if=/dev/zero of=/workspace/probe bs=1M count=32 2>/dev/null" \
        && ok "CONTROL: the unbounded sibling still writes 32 MiB happily — the ENOSPC above was the CEILING, not the node" \
        || bad "CONTROL: the unbounded sibling cannot write either: the node's disk is what ran out and this leg proves nothing about sizeLimitGib"
    free1=$(onnode "df -P / | tail -1 | awk '{print \$4}'")
    note "node root free: ${free0}K → ${free1}K while a 1 GiB ceiling filled"

    # RECLAIM. A ceiling that is not given back is a slow leak of exactly
    # sizeLimitGib per volume ever published on the node.
    $K -n $NS delete pod quota-agent noquota-agent --ignore-not-found --wait=true --timeout=240s >/dev/null 2>&1
    imgs=$(onnode "ls /var/lib/kubelet/plugins/s3.csi.chert.us/volumes/*/tree.img 2>/dev/null | wc -l")
    [ "${imgs:-0}" = "0" ] \
        && ok "the image is gone with its volume — the ceiling is reclaimed at unpublish, not leaked per publish" \
        || bad "$imgs tree image(s) remain under the plugin dir after their pods were deleted"
    loop2=$(onnode "grep -c ' /var/lib/kubelet/plugins/s3.csi.chert.us/volumes/[^ ]*/tree ' /proc/mounts")
    [ "${loop2:-0}" = "0" ] \
        && ok "no tree mount is left on the node" \
        || bad "$loop2 tree mount(s) still on the node after unpublish — a loop device is pinned and its blocks are not free"
    free2=$(onnode "df -P / | tail -1 | awk '{print \$4}'")
    note "node root free after reclaim: ${free2}K"
else
    bad "the quota fixtures never reached Running — S18 made no observation at all"
    $K -n $NS delete pod quota-agent noquota-agent --ignore-not-found --wait=false >/dev/null 2>&1
fi

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
    apply_fx tenants.yaml >/dev/null
fi

# ── S23 (F72) ─────────────────────────────────────────────────────────
# Mountpoint completes a file's upload inside close() only when the
# closing process is the one that opened it. When a PARENT opened the
# output and a CHILD wrote it and closed last, the upload completes
# inside FUSE RELEASE — asynchronously, after the container is gone —
# and the old unpublish order (the released marker and the delete, i.e.
# the SIGTERM, right after the detach) cut it: no object, an incomplete
# multipart upload left behind. The plugin now waits for the mounter to
# exit on its own once the source is detached (workers.quiesceSecs).
#
# The tail is slowed — a 64 MiB part size so the whole 48 MiB file is
# the last part, uploaded at completion, and netem on the worker's
# egress — so that a completion left to RELEASE would outlive the
# container by ~10 s: longer than the old window, shorter than the 30 s
# quiesce.
#
# MEASURED 2026-09-30 (kind, RustFS): this leg CANNOT FAIL on data as
# deployed. Every FUSE request from a tenant carries pid 0 (the tenant's
# processes are outside the mounter's pid namespace: the plugin has no
# hostPID, the worker is its own pod), Mountpoint reads 0 == 0 as the
# opener, and every close — the child's included — completes the upload
# inside FLUSH: the child-written file took the whole shaped 12 s at
# close, and the OLD order (workers.quiesceSecs=0) landed it whole too.
# What this leg pins is the ORDER (the plugin waited for the mounter's
# own exit) and that a child-written file lands whole and clean; the
# data-loss window opens only where the mounter can see the tenant's
# pids (hostPID on the plugin, a shared pid namespace), and a run in
# such a configuration is what would make this leg a falsifier.
leg S23 "F72: a file whose last close came from a CHILD of its opener is whole in the bucket after its pod exits, with no incomplete multipart upload left behind"
$K -n $NS apply -f - >/dev/null <<PODEOF
apiVersion: v1
kind: Pod
metadata: { name: f72-writer, namespace: $NS }
spec:
  serviceAccountName: trainer
  securityContext: { runAsNonRoot: true, runAsUser: 1001, seccompProfile: { type: RuntimeDefault } }
  volumes:
    - name: data
      csi: { driver: s3.csi.chert.us, volumeAttributes: { chert.us/mount: datasets-f72 } }
  containers:
    - name: agent
      image: busybox:1.36
      command: ["/bin/sh", "-c"]
      args: ["trap 'exit 0' TERM INT; sleep 86400 & wait"]
      securityContext: { allowPrivilegeEscalation: false, capabilities: { drop: [ALL] } }
      volumeMounts: [{ name: data, mountPath: /mnt/s3 }]
PODEOF
# A clean prefix: the object, and any incomplete upload an earlier run
# (or a control that ran the old order) left behind — otherwise the
# fingerprint below would read a predecessor's leftovers.
mcx mc rm --force m/$BUCKET/datasets/f72/late.bin >/dev/null 2>&1
mcx mc rm --incomplete --recursive --force m/$BUCKET/datasets/f72/ >/dev/null 2>&1
if wait_phase f72-writer Running 180 && require_pod f72-writer; then
    w=$(worker_of f72-writer)
    pid=$(worker_pid "$w")
    # Amazon Linux 2023 ships no `tc` (iproute-tc is its own package), so
    # the shaping below could never run there — and run 3 on s3a never
    # reached it (2026-09-30). Install it where dnf is; kind's Debian node
    # carries tc already.
    onnode "command -v tc >/dev/null 2>&1 || { command -v dnf >/dev/null 2>&1 && dnf install -q -y iproute-tc >/dev/null 2>&1; }; command -v tc >/dev/null 2>&1" \
        || note "no tc on $NODE and none could be installed — the shaping below will fail"
    # The pid must live in a netns of its own (the pod's), never pid 1's:
    # shaping the node's interface is the mistake the helper's comment
    # describes, and it would make this leg pass against the defect.
    if [ -n "$pid" ] && [ "$(onnode "readlink /proc/$pid/ns/net")" != "$(onnode "readlink /proc/1/ns/net")" ] \
        && onnode "nsenter -t $pid -n tc qdisc add dev eth0 root netem rate 40mbit"; then
        ok "worker $w egress shaped to 40 Mbit/s in the pod's own netns (a 48 MiB tail takes ~10 s)"
    else
        bad "could not shape worker $w's egress in its own netns (pid '$pid'): without it the completion finishes before unpublish and this leg cannot fail"
    fi
    # The opener (this shell) closes its descriptor BEFORE a byte is
    # written; the child writes 48 MiB and is the last to close.
    inpod f72-writer "exec 3>/mnt/s3/late.bin; ( sleep 1; head -c 50331648 /dev/zero >&3 ) & exec 3>&-; wait; echo written" | grep -q written \
        && ok "48 MiB written by a child of the opener, which had already closed" || bad "the writer failed"
    # The container exits NOW, with the completion in flight.
    $K -n $NS delete pod f72-writer --wait=true --timeout=180s >/dev/null 2>&1
    i=0; while [ $i -lt 90 ] && $K -n $WNS get pod "$w" >/dev/null 2>&1; do sleep 2; i=$((i + 2)); done
    $K -n $WNS get pod "$w" >/dev/null 2>&1 && bad "worker $w still exists ${i}s after the tenant was deleted" || ok "worker $w is gone (${i}s)"
    $K -n $SYS logs "$(plugin_pod)" 2>/dev/null | grep -q "exited on its own once its source was detached" \
        && ok "the plugin waited for the mounter's own exit (F72 order)" || bad "no 'exited on its own' line in the plugin log: the old order ran, or the wait hit its ceiling"
    size=$(mcx mc stat --json m/$BUCKET/datasets/f72/late.bin 2>/dev/null | python3 -c 'import json,sys; print(json.load(sys.stdin)["size"])' 2>/dev/null)
    [ "${size:-0}" = "50331648" ] && ok "late.bin is in the bucket whole (48 MiB)" || bad "late.bin in the bucket: size '${size:-absent}' — the completion was cut"
    # The fingerprint below reads an EMPTY listing as success, so the store
    # must be shown to answer ListMultipartUploads at all (a store that
    # refuses it would make the check vacuous).
    mcx mc ls --incomplete m/$BUCKET/datasets/f72/ >/dev/null 2>&1 && ok "the store answers ListMultipartUploads" || bad "the store does not answer ListMultipartUploads — the incomplete-upload check below would be vacuous"
    inc=$(mcx mc ls --incomplete --recursive m/$BUCKET/datasets/f72/ 2>/dev/null | grep -c . || true)
    [ "${inc:-0}" = "0" ] && ok "no incomplete multipart upload under datasets/f72/" || bad "$inc incomplete multipart upload(s) under datasets/f72/ — the fingerprint of a cut completion"
else
    bad "f72-writer never reached Running in 180s — S23 made no observation at all (this was a SILENT skip: run 3 on s3a, 2026-09-30)"
fi

# ── S24 shared read-only mounter ──────────────────────────────────────
# spec.sharing.readOnly: one worker per node for a CR's read-only
# consumers of one class (namespace, CR, uid/gid, credential mode,
# argv). shared-a and shared-b are one class; shared-c asks for uid
# 1002 and is another (the control that the owner is in the key). The
# creator's worker names it in chert.us/tenant-pod; the joiner has no
# worker of its own. The creator leaves FIRST, so the mounter must
# outlive the pod its annotation names; the last member's leave brings
# it down, in F72's order.
leg S24 "sharing: two read-only consumers of one CR on one node share ONE worker, a third with another uid gets its own; the first out leaves the mounter up, the last brings it down"
# The publishes this leg reads from the plugin's log must be THIS
# plugin's: the fixtures were published at setup, and every roll since
# (S9, S17, S17f, and a multi-node rolling update) took that log with
# its pod — 2 BAD on s3a 2026-09-30 for lines the first plugin wrote.
# Recreate the three, so the lines the assertions read are this pod's.
$K -n $NS delete pod shared-a shared-b shared-c --ignore-not-found --wait=true --timeout=180s >/dev/null 2>&1
for p in shared-a shared-b shared-c; do fx_doc tenants.yaml Pod "$p" | $K apply -f - >/dev/null; done
wait_phase shared-a Running 300 && wait_phase shared-b Running 300 && wait_phase shared-c Running 300 \
    || bad "PRECONDITION: shared-a/b/c did not all reach Running after their recreate — the log lines below would be nobody's"
shared_workers() { $K -n $WNS get pods -o json 2>/dev/null | python3 -c "
import json,sys
n=0
for p in json.load(sys.stdin)['items']:
    a=p['metadata'].get('annotations',{})
    if a.get('chert.us/cr')=='datasets-shared' and 'chert.us/shared-mount' in a and p.get('status',{}).get('phase')=='Running' and not p['metadata'].get('deletionTimestamp'): n+=1
print(n)"; }
if require_pod shared-a && require_pod shared-b && require_pod shared-c; then
    got=$(inpod shared-a "cat /mnt/shared/shard-01.txt"); [ "$got" = "seeded-object-01" ] && ok "shared-a reads through the shared mount" || bad "shared-a read '$got'"
    got=$(inpod shared-b "cat /mnt/shared/shard-01.txt"); [ "$got" = "seeded-object-01" ] && ok "shared-b reads through the shared mount" || bad "shared-b read '$got'"
    got=$(inpod shared-c "cat /mnt/shared/shard-01.txt"); [ "$got" = "seeded-object-01" ] && ok "shared-c reads through its own class's mount" || bad "shared-c read '$got'"
    n=$(shared_workers); [ "$n" = "2" ] && ok "two shared workers for datasets-shared: one class for uid 1001 (shared-a + shared-b), one for uid 1002 (shared-c)" || bad "$n shared worker(s) for datasets-shared, expected 2"
    wa=$(worker_of shared-a); wb=$(worker_of shared-b)
    if [ -n "$wa" ] && [ -z "$wb" ]; then creator=shared-a; joiner=shared-b; w=$wa
    elif [ -z "$wa" ] && [ -n "$wb" ]; then creator=shared-b; joiner=shared-a; w=$wb
    else creator=""; fi
    [ -n "$creator" ] && ok "exactly one of the pair created a worker ($creator → $w); the other joined it" || bad "workers named by shared-a '$wa' and shared-b '$wb' — expected exactly one creator"
    srcs=$(onnode "grep -c 'plugins/s3.csi.chert.us/shared/' /proc/mounts"); [ "${srcs:-0}" = "2" ] && ok "two shared FUSE sources on the node (one per class)" || bad "$srcs shared source mount(s) on the node, expected 2"
    # The CR names no cache, and a sharing CR gets none it did not ask
    # for (2026-09-30, sharing design §11 step 1: for one day it got three
    # quarters of the scratch by default, which on the emptyDir's root
    # disk read 6 GiB 5× slower than S3). The argv carries no --cache and
    # mount-s3 made no `mountpoint-cache-<id>` under the scratch root for
    # the reads above.
    if [ -n "$w" ]; then
        argv=$(worker_argv "$w")
        echo "$argv" | grep -qE -- '(^| )--cache( |$)' && bad "$w's mount-s3 runs with a cache the CR never asked for: $argv" || ok "$w's mount-s3 argv carries no --cache (the CR named none)"
        blocks=$($K -n $WNS exec "$w" -- ls -R /tmp 2>/dev/null | grep -c "mountpoint-cache" || true)
        [ "${blocks:-0}" = "0" ] && ok "no block cache directory under /tmp in $w after the reads" || bad "$blocks mountpoint-cache entr(y|ies) under /tmp in $w — a cache the CR did not ask for is in use"
    fi
    plugin_log | grep -q "sharing: no block cache" && ok "the plugin logged that the shared mounter runs without a cache" || bad "no 'sharing: no block cache' line in the plugin log"
    inpod shared-a "touch /mnt/shared/x" >/dev/null 2>&1 && bad "shared-a could write the read-only shared mount" || ok "the shared mount is read-only"
    lines=$($K -n $SYS logs "$(plugin_pod)" 2>/dev/null | grep -c 'published (shared read-only mount)')
    [ "${lines:-0}" -ge 3 ] && ok "the plugin logged $lines shared publishes" || bad "the plugin logged $lines shared publish(es), expected 3"
    if [ -n "$creator" ]; then
        # The creator out first: the mounter its annotation names stays for the joiner.
        $K -n $NS delete pod "$creator" --wait=true --timeout=180s >/dev/null 2>&1
        sleep 3
        [ "$($K -n $WNS get pod "$w" -o jsonpath='{.status.phase}' 2>/dev/null)" = "Running" ] && ok "worker $w still Running after its creator $creator left" || bad "worker $w is not Running after the FIRST member ($creator) left"
        got=$(inpod "$joiner" "cat /mnt/shared/shard-02.txt"); [ "$got" = "seeded-object-02" ] && ok "$joiner still reads after $creator left" || bad "$joiner read '$got' after $creator left"
        $K -n $SYS logs "$(plugin_pod)" 2>/dev/null | grep -q "left the shared mount; its mounter stays" && ok "the plugin recorded the leave" || bad "no 'left the shared mount' line in the plugin log"
        # The last member out: the mounter comes down.
        $K -n $NS delete pod "$joiner" --wait=true --timeout=180s >/dev/null 2>&1
        i=0; while [ $i -lt 90 ] && $K -n $WNS get pod "$w" >/dev/null 2>&1; do sleep 2; i=$((i + 2)); done
        $K -n $WNS get pod "$w" >/dev/null 2>&1 && bad "worker $w still exists ${i}s after the last member left" || ok "worker $w is gone after the last member left (${i}s)"
        $K -n $SYS logs "$(plugin_pod)" 2>/dev/null | grep -q "last member out: bringing the shared mounter down" && ok "the plugin brought the mounter down as the last member left" || bad "no 'last member out' line in the plugin log"
        left=$(onnode "ls /var/lib/kubelet/plugins/s3.csi.chert.us/shared 2>/dev/null | wc -l"); [ "${left:-0}" = "1" ] && ok "one shared record left on the node (shared-c's class)" || bad "shared records on the node: $left, expected 1 (shared-c's)"
    fi
    # CONTROL: the cache still comes when asked. A CR edited under running
    # members is a new class for new members (the argv is in the key), so
    # shared-c — alone in its class — is recreated against the CR with a
    # 256 MiB cache named: the flags must be in its argv and a read must
    # populate the cache directory. Then the CR is restored and shared-c
    # recreated on it, so the legs after this one find the fixture as
    # written. Without this arm a plugin that dropped the cache flags
    # altogether would pass the assertions above.
    $K -n $NS patch fpm datasets-shared --type=merge -p '{"spec":{"cache":{"enabled":true,"maxSizeMib":256}}}' >/dev/null
    $K -n $NS delete pod shared-c --ignore-not-found --wait=true --timeout=180s >/dev/null 2>&1
    fx_doc tenants.yaml Pod shared-c | $K apply -f - >/dev/null
    if wait_phase shared-c Running 300; then
        wc=$(worker_of shared-c); argvc=$(worker_argv "$wc")
        echo "$argvc" | grep -q -- '--cache /tmp --max-cache-size 256' && ok "CONTROL: shared-c's mount-s3 runs --cache /tmp --max-cache-size 256 once the CR asks" || bad "CONTROL: shared-c's argv carries no '--cache /tmp --max-cache-size 256': ${argvc:-<none>}"
        got=$(inpod shared-c "cat /mnt/shared/shard-01.txt"); [ "$got" = "seeded-object-01" ] && ok "CONTROL: shared-c reads through the cached shared mount" || bad "CONTROL: shared-c read '$got' through the cached mount"
        blocks=$($K -n $WNS exec "$wc" -- ls -R /tmp 2>/dev/null | grep -c "mountpoint-cache" || true)
        [ "${blocks:-0}" -ge 1 ] && ok "CONTROL: the named cache is populated under /tmp in $wc ($blocks entries)" || bad "CONTROL: no mountpoint-cache under /tmp in $wc after a read — the named cache did not reach the mounter"
    else
        bad "CONTROL: shared-c did not come back Running on the cached class"
    fi
    $K -n $NS patch fpm datasets-shared --type=json -p '[{"op":"remove","path":"/spec/cache"}]' >/dev/null
    $K -n $NS delete pod shared-c --ignore-not-found --wait=true --timeout=180s >/dev/null 2>&1
    apply_fx tenants.yaml >/dev/null
fi

# ── S25 memory target, pre-pull, the chart's floor ───────────────────
# Fix 1 + fix 3a of the 2026-09-30 passthrough review. Mountpoint 1.24
# targets 95% of the cgroup limit for its buffers and calls that "not a
# guaranteed limit"; the plugin now pins --memory-target at two thirds
# of workers.resources.limits.memory (682 at the rig's 1Gi), a CR that
# names its own keeps it (mount-s3 refuses the flag twice, so the
# plugin's must be ABSENT then), the chart refuses a limit under
# mount-s3's own 512 MiB floor, and the worker image is pulled by the
# plugin's init container before any tenant needs it.
leg S25 "the mounter's memory target is two thirds of the worker limit, a CR's own wins, the worker image was pre-pulled by the plugin, and the chart refuses a limit under 512Mi"
# `reader` was published at setup; the line this leg reads from the
# plugin's log is that publish's, and the plugin has been rolled since
# (1 BAD on s3a 2026-09-30). Recreate it, so the line is this pod's.
$K -n $NS delete pod reader --ignore-not-found --wait=true --timeout=180s >/dev/null 2>&1
fx_doc tenants.yaml Pod reader | $K apply -f - >/dev/null
wait_phase reader Running 300 || bad "PRECONDITION: reader did not come back Running after its recreate"
if require_pod reader; then
    w=$(worker_of reader)
    argv=$(worker_argv "$w")
    echo "$argv" | grep -q -- '--memory-target 682 ' && ok "reader's mount-s3 runs --memory-target 682 (two thirds of the chart's 1Gi limit)" || bad "reader's mount-s3 argv carries no '--memory-target 682': ${argv:-<no mount-s3 cmdline in $w>}"
    plugin_log | grep -q "mounter memory target from the worker limit" && ok "the plugin logged the target it derived" || bad "no 'mounter memory target' line in the plugin log"
    pp=$($K -n $SYS get pod "$(plugin_pod)" -o jsonpath='{range .status.initContainerStatuses[?(@.name=="prepull-passthrough")]}{.state.terminated.exitCode}{end}' 2>/dev/null)
    [ "$pp" = "0" ] && ok "the plugin's prepull-passthrough init container ran the worker image and exited 0 on $NODE" || bad "prepull-passthrough init container: exit '${pp:-absent}'"
    # CONTROL: a CR that names its own target keeps it, and the plugin adds none.
    $K -n $NS patch fpm datasets-ro --type=merge -p '{"spec":{"mountOptions":["--memory-target","900"]}}' >/dev/null
    $K -n $NS delete pod reader-ro --wait=true --timeout=120s >/dev/null 2>&1
    fx_doc tenants.yaml Pod reader-ro | $K apply -f - >/dev/null
    $K -n $NS wait --for=condition=ready pod/reader-ro --timeout=180s >/dev/null 2>&1
    w2=$(worker_of reader-ro); argv2=$(worker_argv "$w2")
    if echo "$argv2" | grep -q -- '--memory-target 900' && ! echo "$argv2" | grep -q -- '--memory-target 682'; then ok "CONTROL: a CR naming its own --memory-target (900) keeps it, and the plugin's 682 is absent"; else bad "CONTROL: reader-ro's argv: ${argv2:-<none>}"; fi
    $K -n $NS patch fpm datasets-ro --type=json -p '[{"op":"remove","path":"/spec/mountOptions"}]' >/dev/null
    $K -n $NS delete pod reader-ro --wait=true --timeout=120s >/dev/null 2>&1
    fx_doc tenants.yaml Pod reader-ro | $K apply -f - >/dev/null
    # The floor, live: an upgrade to a 256Mi limit must fail at render and change nothing.
    rev0=$(helm --kube-context "$CTX" -n $SYS status flint-s3-csi -o json 2>/dev/null | python3 -c 'import json,sys; print(json.load(sys.stdin)["version"])')
    if chart_up --set workers.resources.limits.memory=256Mi >/tmp/s25-helm.log 2>&1; then bad "the chart ACCEPTED a 256Mi worker limit"; else grep -q "below 512Mi" /tmp/s25-helm.log && ok "the chart refused a 256Mi worker limit at render time, naming the floor" || bad "the chart refused 256Mi for another reason: $(tail -1 /tmp/s25-helm.log | cut -c1-160)"; fi
    rev1=$(helm --kube-context "$CTX" -n $SYS status flint-s3-csi -o json 2>/dev/null | python3 -c 'import json,sys; print(json.load(sys.stdin)["version"])')
    [ "$rev0" = "$rev1" ] && ok "the refused upgrade left the release at revision $rev0" || bad "the release moved from revision $rev0 to $rev1 on a refused upgrade"
fi

# ── S26 a worker kubelet cannot admit ────────────────────────────────
# Fix 2. The plugin places a worker with spec.nodeName set, so the
# scheduler reserved no room for it; kubelet admits it against the
# node's allocatable and refuses it on a full node (OutOfmemory), while
# the tenant — already bound here — is never rescheduled. Before: a
# FailedPrecondition and a cleanup, retried by kubelet forever, with
# nothing that said why. Now: a WorkerNotAdmitted event naming the node,
# kubelet's reason and what frees it; and the mount completes once room
# is back, on the SAME tenant pod.
leg S26 "a node with no room for the worker: WorkerNotAdmitted on the tenant naming kubelet's refusal, the pod stays ContainerCreating, and mounts once capacity is back"
if chart_up --set workers.resources.requests.memory=1000Gi --set workers.resources.limits.memory=1000Gi >/dev/null 2>&1 && plugin_rolled; then
    ok "chart upgraded: every new worker now asks kubelet for 1000Gi"
    fx_doc tenants.yaml Pod reader | sed 's/^  name: reader$/  name: noroom/' | $K apply -f - >/dev/null
    i=0; ev=""; while [ $i -lt 150 ]; do ev=$(mount_events noroom | grep WorkerNotAdmitted | head -1); [ -n "$ev" ] && break; sleep 5; i=$((i + 5)); done
    [ -n "$ev" ] && ok "WorkerNotAdmitted landed on noroom within ${i}s" || bad "no WorkerNotAdmitted event on noroom in 150 s: $(mount_events noroom | tail -1 | cut -c1-200)"
    echo "$ev" | grep -q "OutOfmemory" && ok "the event names kubelet's reason (OutOfmemory)" || bad "the event does not name OutOfmemory: $(echo "$ev" | cut -c1-200)"
    echo "$ev" | grep -q "will not be rescheduled" && ok "the event says the tenant stays bound to this node" || bad "the event lacks the rescheduling warning"
    echo "$ev" | grep -q "maxPods" && ok "…and names the node's maxPods as the other ceiling" || bad "the event does not mention maxPods"
    ph=$($K -n $NS get pod noroom -o jsonpath='{.status.phase}'); [ "$ph" = "Pending" ] && ok "noroom is still Pending (ContainerCreating), not Failed" || bad "noroom phase: $ph"
    failed=$($K -n $WNS get pods --field-selector status.phase=Failed --no-headers 2>/dev/null | grep -c . || true)
    [ "${failed:-0}" = "0" ] && ok "no refused worker was left behind in $WNS" || note "$failed Failed worker(s) in $WNS at this instant (the plugin's cleanup and kubelet's GC reap them)"
    chart_up >/dev/null 2>&1 && plugin_rolled && ok "chart restored" || bad "chart restore failed"
    $K -n $NS wait --for=condition=ready pod/noroom --timeout=300s >/dev/null 2>&1 && ok "noroom mounted once the requests fit again — kubelet's retry on the same pod, no recreation" || bad "noroom not Ready 300 s after the chart was restored: $(mount_events noroom | tail -1 | cut -c1-200)"
    got=$(inpod noroom "cat /mnt/s3/shard-02.txt"); [ "$got" = "seeded-object-02" ] && ok "noroom reads content" || bad "noroom read '$got'"
    $K -n $NS delete pod noroom --wait=true --timeout=120s >/dev/null 2>&1
else
    bad "chart upgrade to 1000Gi requests failed; leg skipped"
fi

# ── S27 a worker still pulling at the publish deadline ───────────────
# Fix 3b. NodePublishVolume waits 45 s for the worker to run; at the
# deadline it used to clean up — DELETING the Pending pod — and answer
# Unavailable, so kubelet's retry created it again and waited again,
# under a backoff growing toward two minutes, while containerd's pull
# carried on regardless. Now the worker is kept (phase worker-pending)
# and the retry adopts it. The image that "is still coming" here is a
# tag no node has and no registry serves; it lands when the leg loads
# it, and the SAME worker pod — the uid pins it — must carry the mount.
leg S27 "a worker whose image is still coming at the 45 s deadline is KEPT: kubelet's retries adopt the same pod, and the mount completes on it once the image lands"
if ! image_absent; then
    bad "the registry already serves dilipdalton/flint-s3-worker:pull-test (a previous run's leftover): the worker would pull it at once and this leg would measure nothing"
elif ! node_image_drop; then
    bad "$NODE still holds $HUB_REPO:pull-test in its containerd cache and it could not be dropped: the worker would start from the cache at once and this leg would measure nothing"
elif chart_up --set workers.passthroughImage.tag=pull-test --set workers.prepull.passthrough=false >/dev/null 2>&1 && plugin_rolled; then
    ok "chart upgraded: workers run dilipdalton/flint-s3-worker:pull-test (nowhere yet, checked); pre-pull off so the plugin itself can roll"
    fx_doc tenants.yaml Pod reader | sed 's/^  name: reader$/  name: pull-reader/' | $K apply -f - >/dev/null
    i=0; w=""; while [ $i -lt 60 ]; do w=$(worker_of_any pull-reader); [ -n "$w" ] && break; sleep 2; i=$((i + 2)); done
    [ -n "$w" ] && ok "worker $w created for pull-reader (${i}s)" || bad "no worker for pull-reader in 60 s"
    uid0=$($K -n $WNS get pod "$w" -o jsonpath='{.metadata.uid}' 2>/dev/null)
    note "waiting 150 s: past the 45 s deadline and through at least one kubelet retry"
    sleep 150
    uid1=$($K -n $WNS get pod "$w" -o jsonpath='{.metadata.uid}' 2>/dev/null)
    [ -n "$uid0" ] && [ "$uid0" = "$uid1" ] && ok "the same worker pod (uid $uid0) is still there after 150 s of retries — never deleted and recreated" || bad "worker uid changed or vanished: '$uid0' → '$uid1'"
    plugin_log | grep -q "still coming up at the deadline; kept for kubelet's retry" && ok "the plugin kept the worker at the deadline (logged)" || bad "no 'kept for kubelet's retry' line in the plugin log"
    plugin_log | grep -q "still coming up; resuming rather than starting over" && ok "a retry resumed on the kept worker (logged)" || bad "no 'resuming rather than starting over' line in the plugin log"
    n=$(plugin_log | grep -c "retrying an unfinished publish"); [ "${n:-0}" = "0" ] && ok "no attempt started the publish over (0 cleanup lines)" || bad "$n 'retrying an unfinished publish' cleanup(s) in the plugin log"
    note "worker $w is waiting on: $($K -n $WNS get pod "$w" -o jsonpath='{.status.containerStatuses[0].state.waiting.reason}' 2>/dev/null)"
    image_lands && ok "the pull-test image is now where the nodes pull from ($(image_where)): the same pod's next pull succeeds" || bad "could not make the pull-test tag available ($(image_where))"
    $K -n $NS wait --for=condition=ready pod/pull-reader --timeout=540s >/dev/null 2>&1 && ok "pull-reader mounted once the image was there" || bad "pull-reader not Ready 540 s after the image landed: $(mount_events pull-reader | tail -1 | cut -c1-200)"
    uid2=$($K -n $WNS get pod "$w" -o jsonpath='{.metadata.uid}' 2>/dev/null)
    [ "$uid2" = "$uid0" ] && ok "…on THAT worker (uid unchanged through the whole wait)" || bad "the mount landed on a different worker: '$uid0' → '$uid2'"
    got=$(inpod pull-reader "cat /mnt/s3/shard-02.txt"); [ "$got" = "seeded-object-02" ] && ok "pull-reader reads content" || bad "pull-reader read '$got'"
    $K -n $NS delete pod pull-reader --wait=true --timeout=120s >/dev/null 2>&1
    chart_up >/dev/null 2>&1 && plugin_rolled && ok "chart restored (tag $TAG, pre-pull on)" || bad "chart restore failed"
    # The docker host's tag, and the registry's on a real cluster. NOT
    # `crictl rmi` on the node: containerd's CRI removes the IMAGE, every
    # tag of it, and pull-test is the same image as $TAG — measured on
    # the box 2026-09-30, where a by-hand `crictl rmi …:pull-test` left
    # the worker node with no flint-s3-worker:$TAG at all (the next
    # plugin roll's pre-pull would have failed and blocked the plugin).
    # The node keeps the extra tag.
    docker rmi dilipdalton/flint-s3-worker:pull-test >/dev/null 2>&1
    image_unland
else
    bad "chart upgrade to the pull-test tag failed; leg skipped"
fi

# ── S28 sharing waits for the broker ─────────────────────────────────
# Fix 4. A sharing CR's decision reads the broker's backend; when that
# read failed the plugin used to give the pod a mounter of its OWN — no
# shared cache, no shared pool — for its whole life, with a log line.
# Now it is a retry. The rig can only take the whole broker away (its
# status and its exchange fail together), so what this leg pins is that
# the refusal names the sharing decision and that recovery converges on
# the shared class; the case that separates old from new — a status
# that fails while the exchange works — has no shape on this rig.
leg S28 "a sharing CR whose broker cannot be asked at publish is RETRIED, not given a mounter of its own: once the broker is back the pod joins the shared class"
$K -n $NS wait --for=condition=ready pod/shared-a pod/shared-b --timeout=180s >/dev/null 2>&1
$K -n $SYS scale deploy/flint-s3-broker --replicas=0 >/dev/null 2>&1
$K -n $SYS wait --for=delete pod -l app.kubernetes.io/name=flint-s3-broker --timeout=120s >/dev/null 2>&1
# A fresh plugin: nothing cached answers for the broker.
$K -n $SYS delete pod "$(plugin_pod)" --wait=true --timeout=120s >/dev/null 2>&1; plugin_rolled
fx_doc tenants.yaml Pod shared-a | sed 's/^  name: shared-a$/  name: shared-retry/' | $K apply -f - >/dev/null
i=0; ev=""; while [ $i -lt 120 ]; do ev=$(mount_events shared-retry | grep "asks for sharing and" | head -1); [ -n "$ev" ] && break; sleep 5; i=$((i + 5)); done
[ -n "$ev" ] && ok "the refusal names the sharing decision, not a credential outage: $(echo "$ev" | cut -c1-150)…" || bad "no 'asks for sharing' FailedMount on shared-retry in 120 s: $(mount_events shared-retry | tail -1 | cut -c1-200)"
plugin_log | grep -q "sharing cannot be decided yet" && ok "the plugin logged the retry" || bad "no 'sharing cannot be decided yet' line in the plugin log"
[ -z "$(worker_of_any shared-retry)" ] && ok "no mounter of its own was created for shared-retry during the outage" || bad "shared-retry got its own worker $(worker_of_any shared-retry) during the outage"
$K -n $SYS scale deploy/flint-s3-broker --replicas=1 >/dev/null 2>&1
$K -n $SYS rollout status deploy/flint-s3-broker --timeout=180s >/dev/null 2>&1
$K -n $NS wait --for=condition=ready pod/shared-retry --timeout=300s >/dev/null 2>&1 && ok "shared-retry mounted once the broker was back" || bad "shared-retry not Ready 300 s after the broker returned: $(mount_events shared-retry | tail -1 | cut -c1-200)"
plugin_log | grep "tenant=s3-tenants/shared-retry" | tail -1 | grep -q "published (shared read-only mount)" && ok "…as a MEMBER of the shared class, not with a mounter of its own" || bad "shared-retry's publish line: $(plugin_log | grep 'tenant=s3-tenants/shared-retry' | tail -1 | cut -c1-200)"
[ -z "$(worker_of shared-retry)" ] && ok "shared-retry created no worker (it joined the class's)" || bad "shared-retry has a worker of its own: $(worker_of shared-retry)"
got=$(inpod shared-retry "cat /mnt/shared/shard-03.txt"); [ "$got" = "seeded-object-03" ] && ok "shared-retry reads through the shared mount" || bad "shared-retry read '$got'"
$K -n $NS delete pod shared-retry --wait=true --timeout=120s >/dev/null 2>&1

# ── S29 a refused member of a shared class ───────────────────────────
# Fix 5. A refused refresh (the SA dropped from consumers) removed the
# worker's creds.json so the door answers 503 and the client fails at
# expiry — right for a mounter of the pod's own, wrong for a shared
# class, whose ONE file serves every member: the siblings' mount-s3,
# refreshing inside the window before the next member's republish
# re-minted the key, got a 503 and an EIO, while the refused pod — bound
# to the same superblock — read on regardless. Now the file stays. The
# file is sampled every 2 s for 100 s after the refusal, longer than a
# republish period: the old code's removal-then-re-mint would show as
# absent samples.
leg S29 "revoking ONE member of a shared class refuses its refresh without taking the class's credential from the others: the file stays, the siblings keep reading"
$K -n $NS wait --for=condition=ready pod/shared-a pod/shared-b --timeout=180s >/dev/null 2>&1
$K -n $NS create sa trainer2 >/dev/null 2>&1 || true
$K -n $NS patch fpm datasets-shared --type=merge -p '{"spec":{"consumers":{"serviceAccounts":["trainer","trainer2"]}}}' >/dev/null
fx_doc tenants.yaml Pod shared-a | sed -e 's/^  name: shared-a$/  name: shared-d/' -e 's/serviceAccountName: trainer$/serviceAccountName: trainer2/' | $K apply -f - >/dev/null
$K -n $NS wait --for=condition=ready pod/shared-d --timeout=180s >/dev/null 2>&1 && ok "shared-d (SA trainer2) mounted" || bad "shared-d not Ready in 180 s: $(mount_events shared-d | tail -1 | cut -c1-200)"
plugin_log | grep "tenant=s3-tenants/shared-d" | tail -1 | grep -q "published (shared read-only mount)" && ok "shared-d joined the shared class (a second SA in one class)" || bad "shared-d's publish line: $(plugin_log | grep 'tenant=s3-tenants/shared-d' | tail -1 | cut -c1-200)"
w=$(worker_of_any shared-a); [ -n "$w" ] || w=$(worker_of_any shared-b); [ -n "$w" ] || w=$(worker_of_any shared-d)
wuid=$($K -n $WNS get pod "$w" -o jsonpath='{.metadata.uid}' 2>/dev/null)
comm="/var/lib/kubelet/pods/$wuid/volumes/kubernetes.io~empty-dir/comm"
onnode "test -f $comm/creds.json" && ok "the class's creds.json is in $w's comm dir on the node" || bad "no creds.json at $comm on the node"
$K -n $NS patch fpm datasets-shared --type=merge -p '{"spec":{"consumers":{"serviceAccounts":["trainer"]}}}' >/dev/null
note "trainer2 dropped from consumers; shared-d's next refresh (every republish at lifetime ${CREDS_LIFETIME}s) is refused"
i=0; ev=""; while [ $i -lt 240 ]; do ev=$(mount_events shared-d | grep CredentialRefreshFailed | head -1); [ -n "$ev" ] && break; sleep 5; i=$((i + 5)); done
[ -n "$ev" ] && ok "CredentialRefreshFailed landed on shared-d after ${i}s" || bad "no CredentialRefreshFailed on shared-d in 240 s"
echo "$ev" | grep -q "class's credential stays" && ok "the event says the class's credential stays for the others" || bad "the event lacks the shared wording: $(echo "$ev" | cut -c1-200)"
gone=0; j=0; while [ $j -lt 100 ]; do onnode "test -f $comm/creds.json" || gone=$((gone + 1)); sleep 2; j=$((j + 2)); done
[ "$gone" = "0" ] && ok "creds.json stayed in place through 50 samples over 100 s after the refusal" || bad "creds.json was ABSENT in $gone of 50 samples after the refusal — the class's key was taken away"
got=$(inpod shared-a "cat /mnt/shared/shard-05.txt"); [ "$got" = "seeded-object-05" ] && ok "shared-a (still a consumer) reads an object it had not cached" || bad "shared-a read '$got' after its sibling's refusal"
got=$(inpod shared-d "cat /mnt/shared/shard-06.txt"); [ "$got" = "seeded-object-06" ] && ok "shared-d keeps reading until it exits (per-member revocation is not possible under sharing, as the CRD says)" || note "shared-d read '$got'"
$K -n $NS delete pod shared-d --wait=true --timeout=120s >/dev/null 2>&1
$K -n $NS delete sa trainer2 >/dev/null 2>&1
apply_fx tenants.yaml >/dev/null

# ── S21 (audit 2026-09-03, finding 4) ─────────────────────────────────
# A node reboot empties the worker's memory-backed comm dir: the
# supervisor restarts with no launch record, sits in its accept loop,
# and the pod reads Running with NO syncer inside it — "alive" to the
# plugin forever, nothing publishing. The shape is reproduced without a
# reboot: erase the record and kill the syncer, so the container's
# restart lands exactly there. The plugin must notice a listening
# supervisor with no record and send the launch again — to the SAME pod.
leg S21 "a Running worker with no syncer inside it (its launch record gone, the node-reboot shape) gets the launch sent again on the next republish, in the SAME pod"
$K -n $NS delete pod lean-agent lean-agent2 --ignore-not-found --wait=true --timeout=240s >/dev/null 2>&1
apply_fx lean-agent.yaml >/dev/null
if wait_phase lean-agent Running 300; then
    w=$(worker_of lean-agent)
    wuid0=$($K -n $WNS get pod "$w" -o jsonpath='{.metadata.uid}' 2>/dev/null)
    rc0=$($K -n $WNS get pod "$w" -o jsonpath='{.status.containerStatuses[0].restartCount}' 2>/dev/null)
    $K -n $WNS exec "$w" -- /bin/sh -c 'rm -f /comm/launch.json; for p in /proc/[0-9]*; do [ "$(cat $p/comm 2>/dev/null)" = flint-sync ] && kill -9 "${p#/proc/}"; done; exit 0' >/dev/null 2>&1
    i=0; rc1=$rc0
    while [ $i -lt 90 ] && [ "${rc1:-0}" = "${rc0:-0}" ]; do
        sleep 3; i=$((i + 3))
        rc1=$($K -n $WNS get pod "$w" -o jsonpath='{.status.containerStatuses[0].restartCount}' 2>/dev/null)
    done
    [ "${rc1:-0}" -gt "${rc0:-0}" ] \
        && ok "PRECONDITION: the worker container restarted (${rc0:-0} → ${rc1:-0}) with no launch record" \
        || bad "PRECONDITION: the container did not restart in ${i}s — the shape was never produced"
    sleep 5
    nsy=$($K -n $WNS exec "$w" -- /bin/sh -c 'ps | grep -c "[f]lint-sync run"' 2>/dev/null)
    [ "${nsy:-0}" = "0" ] \
        && ok "PRECONDITION: the pod is Running with NO flint-sync inside it — the shape a reboot leaves" \
        || bad "PRECONDITION: flint-sync came back on its own ($nsy) — the launch record was not gone, and this leg tests nothing"
    i=0; back=0
    while [ $i -lt 240 ]; do
        nsy=$($K -n $WNS exec "$w" -- /bin/sh -c 'ps | grep -c "[f]lint-sync run"' 2>/dev/null)
        [ "${nsy:-0}" -ge 1 ] && { back=1; break; }
        sleep 5; i=$((i + 5))
    done
    [ "$back" = "1" ] \
        && ok "flint-sync is back in the worker ${i}s later: the plugin sent the launch again" \
        || bad "no flint-sync in the worker after ${i}s: a Running pod with no syncer is 'alive' forever"
    wuid1=$($K -n $WNS get pod "$w" -o jsonpath='{.metadata.uid}' 2>/dev/null)
    [ -n "$wuid1" ] && [ "$wuid1" = "$wuid0" ] \
        && ok "the SAME pod (uid unchanged): the launch was re-sent, the worker was not recreated" \
        || bad "the worker was recreated ($wuid0 → ${wuid1:-gone}) instead of re-launched"
    $K -n $WNS exec "$w" -- test -f /comm/launch.json 2>/dev/null \
        && ok "the launch record is persisted again" \
        || bad "no launch.json in the worker after the re-send"
    mount_events lean-agent | grep -q 'SyncerRelaunched' \
        && ok "the tenant's events name it (SyncerRelaunched)" \
        || bad "no SyncerRelaunched event on lean-agent"
    # Under the per-barrier fence nothing renews between barriers (a
    # renewal here was the lifetime lease's sign of life — 1 BAD on s3a
    # 2026-09-30); the relaunched syncer is proven live by a publish it
    # must claim for.
    tsh lean-agent "printf 'after the re-send' > /workspace/src/s21.txt"
    out=$(lpublish lean-agent "s21-$(date +%s)" 120); el=${out%% *}; ack=${out#* }
    [ "$(ack_status "$ack")" = "ok" ] && lmhas tenants/proj src/s21.txt \
        && ok "the relaunched syncer claims and publishes (acked in ${el}s, src/s21.txt cited)" \
        || bad "the relaunched syncer did not publish: ack ${el}s '$(printf '%s' "$ack" | cut -c1-100)'"
else
    bad "lean-agent never reached Running in 300s — S21 made no observation at all"
fi
$K -n $NS delete pod lean-agent --ignore-not-found --wait=true --timeout=240s >/dev/null 2>&1

# ── S22 (audit 2026-09-03, finding 5) ─────────────────────────────────
# The operator's refuse-foreign was ADVISORY on the data plane: a CR the
# operator had not yet judged resolved to its spec, and the syncer
# checked out and republished over another project's prefix. The claim
# cell is durable in the bucket, so the SYNCER now reads it before its
# first claim step — with the project id stamped into its environment.
# This rig runs no operator, which is the point: the refusal must not
# depend on one.
leg S22 "lean claim precondition: a prefix whose claim cell names ANOTHER project is refused by the syncer itself, before any checkout — and the same pod mounts once the claim is its own"
$K -n $NS delete pod lean-claimed --ignore-not-found --wait=true --timeout=240s >/dev/null 2>&1
delete_fx lean-claimed.yaml --ignore-not-found --wait=true >/dev/null 2>&1
mcx mc rm --recursive --force m/$BUCKET/tenants/proj-claimed/ >/dev/null 2>&1
mcx sh -c "printf '{\"project_id\":\"team-b/other\",\"created_unix\":1,\"stamped_by\":\"drill\"}' | mc pipe m/$BUCKET/tenants/proj-claimed/.flint/lean/claim" >/dev/null 2>&1
[ "$(lobj tenants/proj-claimed/.flint/lean/claim | jq -r .project_id 2>/dev/null)" = "team-b/other" ] \
    && ok "PRECONDITION: a standing claim by team-b/other is in the bucket" \
    || bad "PRECONDITION: the foreign claim was not planted — this leg tests nothing"
apply_fx lean-claimed.yaml >/dev/null
if wait_phase lean-claimed Running 150; then
    bad "lean-claimed is Running over a prefix claimed by another project: refuse-foreign is still advisory on the data plane"
else
    ev=$(mount_events lean-claimed)
    echo "$ev" | grep -q 'team-b/other' && echo "$ev" | grep -q 'team-a/proj-claimed' \
        && ok "the mount is refused, and the event names BOTH projects" \
        || bad "refusal event: $(echo "$ev" | tail -1 | cut -c1-240)"
    [ "$(lcount tenants/proj-claimed/files/)" = "0" ] \
        && ok "nothing was published under the foreign prefix" \
        || bad "objects appeared under the foreign prefix"
    # The refusal is FINAL for the delivery: exit 78 (flint_lean::
    # EXIT_REFUSED = worker::SYNCER_EXIT_REFUSED) is torn down and named,
    # never relaunched. The first run of this leg found the two crates'
    # contract missing: the syncer exited 1, OnFailure restarted it, the
    # supervisor relaunched it from launch.json, and the plugin told the
    # tenant "checkout in progress" for the whole 150 s. Kubelet retries
    # the mount on a backoff, and each retry makes a worker that lives a
    # few seconds before the plugin tears it down — so the observation
    # is an instant with NO Running worker, not a single sample.
    echo "$ev" | grep -q 'SyncerRefused' \
        && ok "the plugin raised its own SyncerRefused event (the reason survives kubelet cutting the mount message)" \
        || bad "no SyncerRefused event on the tenant: $(echo "$ev" | grep -c . ) events, none from the plugin's refusal arm"
    echo "$ev" | grep -q 'exited 78' \
        && ok "the refusal carried exit 78: the syncer's EXIT_REFUSED and the plugin's SYNCER_EXIT_REFUSED agree" \
        || bad "the mount events never mention exit 78 — the two crates' refusal code disagrees, or the plugin classified it as something else"
    i=0; gone=""
    while [ $i -lt 90 ]; do
        [ -z "$(worker_of lean-claimed)" ] && { gone=$i; break; }
        sleep 3; i=$((i + 3))
    done
    [ -n "$gone" ] \
        && ok "the refused syncer is torn down, not left crash-looping (no Running worker for lean-claimed at ${gone}s)" \
        || bad "a syncer is still Running for the refused workspace after 90s: the refusal is being relaunched in place"
fi
# CONTROL: the claim becomes our own UNDER THE SAME TENANT POD — no
# delete, no re-apply. Kubelet is still retrying the mount on its
# backoff (up to ~2 min between attempts); the next attempt after the
# flip makes a worker whose syncer finds its own claim, checks out, and
# the tenant reaches Running. The check is the claim, not the pod: a
# refusal must not need an operator to restart anything once it is
# fixed.
tuid0=$($K -n $NS get pod lean-claimed -o jsonpath='{.metadata.uid}' 2>/dev/null)
mcx sh -c "printf '{\"project_id\":\"team-a/proj-claimed\",\"created_unix\":1,\"stamped_by\":\"drill\"}' | mc pipe m/$BUCKET/tenants/proj-claimed/.flint/lean/claim" >/dev/null 2>&1
apply_fx lean-claimed.yaml >/dev/null
if wait_phase lean-claimed Running 300; then
    tuid1=$($K -n $NS get pod lean-claimed -o jsonpath='{.metadata.uid}' 2>/dev/null)
    [ -n "$tuid0" ] && [ "$tuid0" = "$tuid1" ] \
        && ok "CONTROL: with its OWN claim standing the SAME tenant pod mounts on kubelet's next retry (uid unchanged; nothing was restarted)" \
        || bad "CONTROL: lean-claimed mounted but as a different pod ($tuid0 → $tuid1): something recreated it"
else
    bad "CONTROL: lean-claimed did not mount under its own claim within 300s: $(mount_events lean-claimed | tail -1 | cut -c1-200)"
fi
$K -n $NS delete pod lean-claimed --ignore-not-found --wait=true --timeout=240s >/dev/null 2>&1
delete_fx lean-claimed.yaml --ignore-not-found --wait=false >/dev/null 2>&1
mcx mc rm --recursive --force m/$BUCKET/tenants/proj-claimed/ >/dev/null 2>&1

# ── S16 termination ordering, WITHOUT a PodDisruptionBudget ──────────
# LAST on purpose: it drains the node for real, and --force deletes the
# rig's bare pods, which do not come back. Re-run `setup` after.
#
# There is deliberately no budget for workers (see the chart). A PDB
# covers the eviction path only, stalls autoscaler scale-down and blocks
# drains; the drivers with this same architecture answered it with
# ordering instead (awslabs/mountpoint-s3-csi-driver#607 ships graceful
# eviction; juicedata/juicefs-csi-driver#856 is the same failure on a
# drained spot node). What orders the two
# deaths here is a preStop hook — kubelet runs it BEFORE the SIGTERM on
# every termination path — plus a PriorityClass for kubelet's graceful
# node shutdown, which never consults a budget at all.
leg S16 "termination ordering: an EVICTED worker keeps serving until its tenant is released, the priority ranks it above tenants, and the drain completes with no mount left behind"
n=$($K -n $WNS get pdb -o name 2>/dev/null | grep -c . || true)
[ "${n:-0}" = "0" ] && ok "there is NO PodDisruptionBudget for workers (the ordering is the hook, not a budget)" || bad "$n PodDisruptionBudget(s) in $WNS — a budget has crept back and this leg would prove the wrong mechanism"
# Kubelet's graceful shutdown terminates by priority, lowest first, and
# never consults a budget. A worker must outrank the tenant it serves.
if require_pod reader; then
    w=$(worker_of reader)
    wp=$($K -n $WNS get pod "$w" -o jsonpath='{.spec.priority}' 2>/dev/null)
    tp=$($K -n $NS get pod reader -o jsonpath='{.spec.priority}' 2>/dev/null)
    [ -n "$wp" ] && [ -n "$tp" ] && [ "$wp" -gt "$tp" ] \
        && ok "the worker outranks its tenant for node shutdown (worker $wp > tenant $tp)" \
        || bad "worker priority '$wp' does not outrank tenant '$tp' — on a reboot the order is a coin flip"
    # THE ORDERING ITSELF. Evict the worker with its tenant still live:
    # the eviction is ACCEPTED (no budget refuses it), the pod enters
    # termination — and the preStop hook holds the container open, so
    # the tenant must keep reading. Without the hook the container is
    # SIGTERMed at once and this read returns nothing.
    sum0=$(inpod reader "cat /mnt/s3/shard-0*.txt | md5sum | cut -c1-32")
    [ -n "$sum0" ] && ok "PRECONDITION: reader reads its mount before the eviction ($sum0)" || bad "PRECONDITION: reader reads nothing already"
    f=$(mktemp); printf '{"apiVersion":"policy/v1","kind":"Eviction","metadata":{"name":"%s","namespace":"%s"}}' "$w" "$WNS" > "$f"
    out=$($K create --raw "/api/v1/namespaces/$WNS/pods/$w/eviction" -f "$f" 2>&1); rm -f "$f"
    case "$out" in
        *TooManyRequests*) bad "the eviction was REFUSED — something is still budgeting workers: $(printf '%s' "$out" | tr -d '\n' | cut -c1-100)" ;;
        *) ok "the eviction is accepted (nothing refuses it; the ordering is not a refusal)" ;;
    esac
    dts=$($K -n $WNS get pod "$w" -o jsonpath='{.metadata.deletionTimestamp}' 2>/dev/null)
    [ -n "$dts" ] && ok "the worker is terminating (deletionTimestamp $dts) — so what follows is the hook holding it, not a no-op" || bad "the worker has no deletionTimestamp; the eviction did not start termination and the read below proves nothing"
    sleep 10
    sum1=$(inpod reader "cat /mnt/s3/shard-0*.txt | md5sum | cut -c1-32")
    [ "$sum1" = "$sum0" ] && ok "10s into its own termination the worker is STILL SERVING its tenant ($sum1) — the preStop hook is the ordering" || bad "the tenant lost its mount while the worker was still terminating: '$sum1' vs '$sum0'"
    $K -n $WNS get pod "$w" >/dev/null 2>&1 && ok "the worker has not exited while its volume is still published" || bad "worker $w exited during its preStop window"
    # Release it the way NodeUnpublish does: the tenant goes.
    $K -n $NS delete pod reader --wait=true --timeout=180s >/dev/null 2>&1
    i=0; while [ $i -lt 90 ] && $K -n $WNS get pod "$w" >/dev/null 2>&1; do sleep 3; i=$((i + 3)); done
    $K -n $WNS get pod "$w" >/dev/null 2>&1 && bad "worker $w outlived its tenant by ${i}s — the hook is not being released" || ok "the worker followed its tenant ${i}s after the tenant was deleted"
fi
# THE DRAIN. --force because the rig's tenants are bare pods; the workers
# do not need it (their Node ownerReference makes them managed). Nothing
# refuses an eviction here, so a drain that hangs would be a real defect.
# SCOPED TO THE NODE BEING DRAINED. Counting $WNS cluster-wide is the
# same number on kind, where there is one node, and a FALSE FAILURE on
# real nodes: a worker is pinned to its tenant's node, so one serving a
# live tenant on ANOTHER node is the product working, not residue. Seen
# on runcr 2026-09-10: the drain of runcr-aws-1 completed and this
# reported "1 worker pod(s) still resident" for s3w-6a6cf60ca7d8e25d,
# which was on runcr-aws-2 serving a Running s3-tenants/reader-elsewhere.
before=$($K -n $WNS get pods --no-headers --field-selector spec.nodeName="$NODE" 2>/dev/null | grep -c . || true)
note "draining $NODE with $before worker(s) resident ON IT"
if $K drain "$NODE" --ignore-daemonsets --delete-emptydir-data --force --timeout=420s >/tmp/s16-drain.log 2>&1; then
    ok "the drain COMPLETED — the hooks delayed each worker without blocking the drain"
else
    bad "drain did not complete in 420s: $(tail -2 /tmp/s16-drain.log | tr -d '\n' | cut -c1-140)"
fi
left=$($K -n $WNS get pods --no-headers --field-selector spec.nodeName="$NODE" 2>/dev/null | grep -c . || true)
[ "${left:-0}" = "0" ] && ok "no worker pods remain on $NODE after the drain" || bad "$left worker pod(s) still resident ON $NODE after a completed drain"
stale=$(onnode "grep -c 'plugins/s3.csi.chert.us/volumes' /proc/mounts")
[ "${stale:-0}" = "0" ] && ok "no plugin-dir mount is orphaned on the node" || bad "$stale plugin-dir mount(s) left on the node after the drain"
$K uncordon "$NODE" >/dev/null 2>&1 && note "node uncordoned"

# ── roster ────────────────────────────────────────────────────────────
echo
for want in S1 S2 S3 S4 S5 S5c S6 S7 S8 S9 S10 S11 S12 S13 S14 S15 S16 S17 S17f S18 S19 S20 S21 S22 S23 S24 S25 S26 S27 S28 S29 SU; do
    echo " $RAN_LEGS " | grep -q " $want " || bad "leg $want never ran"
done
echo "════════════════════════════════════════"
echo "s3.csi.chert.us e2e: $PASS ok, $FAILED bad"
[ "$FAILED" = "0" ]
