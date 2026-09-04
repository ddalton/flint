#!/usr/bin/env bash
# s3.csi.chert.us on REAL nodes: two all-spot EC2 clusters from trove, one
# real bucket. The legs are run-s3csi.sh's and multi/run-multi.sh's,
# unchanged — this file is only the substrate: provision, wait, wire the
# kubeconfigs, run, tear down. What the kind rig cannot show and this
# can: a kubelet that is not a container, a node the plugin cannot
# `docker exec` into, spot nodes, and S3 that is S3.
#
#   ./aws-drill.sh provision   # two all-spot clusters (A, B) via trove
#   ./aws-drill.sh wait        # until both kubeconfigs exist and every node is Ready
#   ./aws-drill.sh single      # run-s3csi.sh setup + every leg, on A
#   ./aws-drill.sh multi       # run-multi.sh setup + M1-M3, across A and B
#   ./aws-drill.sh teardown    # drill teardown in both, then the clusters
#   ./aws-drill.sh status
#   ./aws-drill.sh wire        # rewrite the kubeconfig copies (unique cluster/user/context names)
#
# Needs: trove running (~/github/trove, scripts/aws-live-allspot.fish —
# the control plane is spot too, per the standing directive), the
# rolesanywhere profile for provisioning, images PUSHED under TAG
# (build-images.sh PUSH=1 ARCH=amd64), and a bucket: BUCKET, S3_REGION and
# S3_KEY_FILE (the JSON from `aws iam create-access-key` for a user scoped
# to that one bucket — made with trove-admin, deleted with the drill;
# rolesanywhere can neither create a bucket nor list them).
set -u
cd "$(dirname "$0")"
A=${A:-s3a}; B=${B:-s3b}
WORKERS=${WORKERS:-1}
INSTANCE=${INSTANCE:-i4i.large}
TROVE=${TROVE:-$HOME/github/trove}
WORK=${WORK:-/tmp/flint-s3csi-aws}
S3_REGION=${S3_REGION:-us-west-1}
mkdir -p "$WORK"

need() { for v in "$@"; do [ -n "${!v:-}" ] || { echo "$v is required" >&2; exit 2; }; done; }
kc_raw() { echo "/tmp/trove-aws-kc-$1"; }
# A copy with the CLUSTER, USER and CONTEXT all named after the cluster.
# Every trove kubeconfig calls its cluster `kubernetes`, its user
# `kubernetes-admin` and its context `kubernetes-admin@kubernetes`;
# run-multi.sh addresses the two clusters by context over a merged
# KUBECONFIG, and a merge keeps the FIRST file's entry for each name —
# renaming only the context left `s3b` pointing at s3a's API server (the
# first multi run installed both halves on one cluster and reported a
# missing DaemonSet on the other). Entries have to be unique by name.
kc_wire() {
    local f; f="$WORK/kc-$1"
    python3 - "$(kc_raw "$1")" "$f" "$1" <<'PY'
import sys, yaml
src, dst, name = sys.argv[1:4]
d = yaml.safe_load(open(src))
d["clusters"][0]["name"] = name
d["users"][0]["name"] = f"{name}-admin"
d["contexts"] = [{"name": name, "context": {"cluster": name, "user": f"{name}-admin"}}]
d["current-context"] = name
with open(dst, "w") as out:
    yaml.safe_dump(d, out)
PY
    chmod 600 "$f"
    echo "$f"
}
ready() { # name expected-nodes
    local f; f=$(kc_raw "$1"); [ -f "$f" ] || return 1
    local n; n=$(kubectl --kubeconfig "$f" get nodes --no-headers 2>/dev/null | awk '$2=="Ready"' | wc -l | tr -d ' ')
    [ "${n:-0}" -ge "$2" ]
}

case "${1:-}" in
    provision)
        for c in "$A" "$B"; do
            echo "── provisioning $c: $WORKERS worker(s) + spot control plane, $INSTANCE, $S3_REGION"
            TROVE_AWS_DEFAULT_INSTANCE_TYPE="$INSTANCE" TROVE_AWS_DEFAULT_REGION="$S3_REGION" \
                fish "$TROVE/scripts/aws-live-allspot.fish" create "$c" "$WORKERS"
        done
        ;;
    wait)
        want=$((WORKERS + 1)); i=0
        until ready "$A" "$want" && ready "$B" "$want"; do
            [ $i -ge 2400 ] && { echo "clusters not Ready after 40 min" >&2; exit 1; }
            sleep 20; i=$((i + 20))
        done
        for c in "$A" "$B"; do
            echo "── $c ($(kc_wire "$c"))"; kubectl --kubeconfig "$(kc_raw "$c")" get nodes -o wide
        done
        ;;
    status)
        for c in "$A" "$B"; do fish "$TROVE/scripts/aws-live-drive.fish" status "$c"; done
        ;;
    wire)
        for c in "$A" "$B"; do f=$(kc_wire "$c"); printf '%s -> %s: ' "$c" "$f"; kubectl --kubeconfig "$f" get nodes -o jsonpath='{.items[*].metadata.name}'; echo; done
        ;;
    single)
        need TAG BUCKET S3_KEY_FILE
        f=$(kc_wire "$A")
        export KUBECONFIG=$f CTX=$A STORE=s3 NODE_EXEC=nodesh TAG BUCKET S3_REGION S3_KEY_FILE
        ./run-s3csi.sh setup || exit $?
        ./run-s3csi.sh
        ;;
    multi)
        need TAG BUCKET S3_KEY_FILE
        fa=$(kc_wire "$A"); fb=$(kc_wire "$B")
        export KUBECONFIG=$fa:$fb STORE=s3 X1=$A X2=$B TAG BUCKET S3_REGION S3_KEY_FILE
        multi/run-multi.sh setup || exit $?
        multi/run-multi.sh
        ;;
    teardown)
        if [ -f "$(kc_raw "$A")" ]; then
            fa=$(kc_wire "$A"); fb=$(kc_wire "$B" 2>/dev/null || true)
            KUBECONFIG=$fa CTX=$A STORE=s3 NODE_EXEC=nodesh BUCKET=${BUCKET:-x} AWS_ACCESS_KEY_ID=x AWS_SECRET_ACCESS_KEY=x ./run-s3csi.sh teardown || true
            [ -n "$fb" ] && KUBECONFIG=$fa:$fb STORE=s3 X1=$A X2=$B BUCKET=${BUCKET:-x} AWS_ACCESS_KEY_ID=x AWS_SECRET_ACCESS_KEY=x multi/run-multi.sh teardown || true
        fi
        for c in "$A" "$B"; do fish "$TROVE/scripts/aws-live-drive.fish" teardown "$c"; done
        echo "── orphans (should be none)"; fish "$TROVE/scripts/aws-live-drive.fish" orphans || true
        ;;
    *) echo "usage: aws-drill.sh provision|wait|status|wire|single|multi|teardown" >&2; exit 2 ;;
esac
