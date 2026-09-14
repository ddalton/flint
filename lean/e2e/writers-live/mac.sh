#!/usr/bin/env bash
# writers-live — the MAC side of the drill (plan §2, §5, §8): build the
# artifacts from the committed sha, stage them, provision through trove,
# drive every node over SSM, judge each leg, and tear down.
#
#   mac.sh artifacts               binaries (from HEAD, refusing a dirty or stale build),
#                                  the three derived images, the rendered charts → $STAGE
#   mac.sh bucket                  the drill bucket + the node role's inline policy (trove-admin)
#   mac.sh stage                   scripts, binaries, charts, images → s3://$BUCKET/_rig/
#   mac.sh provision --yes         trove: 1 CP + 3 workers, all i4i.large spot, us-west-1
#   mac.sh nodes                   instance ids of the cluster → $STAGE/nodes.tsv
#   mac.sh ssm <id|cp|all|workers> <command…>
#                                  run as root, wait, print (full output via S3)
#   mac.sh prep                    every node: images imported + content-verified, shipper up;
#                                  CP: drill.sh fetch, podwatch up
#   mac.sh cp <drill.sh args…>     a short drill phase on the CP, foreground
#   mac.sh leg <A1|A2|A3|A4>       a storm on the CP in the background, polled to its end
#   mac.sh verdict <leg>           flush evidence, pull the leg, build traces, run the oracles
#   mac.sh status                  one line per node: shipper, disk, pods
#
# Teardown is teardown.sh, run by hand. Nothing here deletes a bucket, a
# policy or a project.
set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../../.." && pwd)
CLUSTER=${CLUSTER:-runcv}
BUCKET=${BUCKET:-flint-lean-writers-20260914}
REGION=${REGION:-us-west-1}
STAGE=${STAGE:-/private/tmp/claude-503/writers-stage}
ADMIN_PROFILE=${ADMIN_PROFILE:-trove-admin}
NODE_ROLE=${NODE_ROLE:-TroveSSMInstanceProfile}
POLICY=${POLICY:-writers-live-bucket}
TRIPLE=x86_64-unknown-linux-musl
# trove-admin throughout: rolesanywhere is denied PutObject on the drill bucket.
export AWS_PROFILE=${AWS_PROFILE:-trove-admin} AWS_REGION=$REGION AWS_DEFAULT_REGION=$REGION
SHA=$(git -C "$REPO" rev-parse --short=8 HEAD)
TAG=writers-$SHA

log() { echo "$(date +%H:%M:%S) $*" >&2; }
die() { echo "$(date +%H:%M:%S) FATAL: $*" >&2; exit 1; }
admin() { AWS_PROFILE=$ADMIN_PROFILE aws "$@"; }

# ── artifacts ─────────────────────────────────────────────────────────
BINS="lean/syncer:flint-sync lean/gateway:flint-lean-gateway spdk-csi-driver:flint-s3-csi-node spdk-csi-driver:flint-s3-broker spdk-csi-driver:flint-lean-operator"

artifacts() {
  # The images must carry a sha: nothing that builds into them may differ
  # from HEAD, and every binary must be newer than HEAD's commit.
  local dirty
  dirty=$(git -C "$REPO" status --porcelain -- lean/syncer lean/gateway crates/flint-store spdk-csi-driver/src spdk-csi-driver/Cargo.toml | grep -v '^??')
  [ -z "$dirty" ] || die "the build inputs differ from HEAD:\n$dirty"
  local commit_t pair crate bin src
  commit_t=$(git -C "$REPO" log -1 --format=%ct)
  mkdir -p "$STAGE/bin" "$STAGE/images" "$STAGE/charts"
  for pair in $BINS; do
    crate=${pair%%:*}; bin=${pair#*:}
    src=$REPO/$crate/target/$TRIPLE/release/$bin
    [ -f "$src" ] || die "missing $src"
    [ "$(stat -f %m "$src")" -ge "$commit_t" ] || die "$bin is older than HEAD ($SHA): rebuild"
    cp "$src" "$STAGE/bin/$bin"
  done
  (cd "$STAGE/bin" && shasum -a 256 * > "$STAGE/bin.SHA256SUMS")
  cat "$STAGE/bin.SHA256SUMS" >&2

  local B=$STAGE/bin
  python3 "$HERE/oci_derive.py" --base dilipdalton/flint-s3-worker-lean:1.51.0 --platform linux/amd64 \
    --add "/usr/local/bin/flint-sync=$B/flint-sync" \
    --label "us.chert.drill-sha=$SHA" \
    --tag "docker.io/dilipdalton/flint-s3-worker-lean:$TAG" --out "$STAGE/images/worker-lean.tar" \
    > "$STAGE/images/worker-lean.json" || die "derive worker-lean"
  python3 "$HERE/oci_derive.py" --base dilipdalton/flint-s3-csi:1.51.0 --platform linux/amd64 \
    --add "/usr/local/bin/flint-s3-csi-node=$B/flint-s3-csi-node" \
    --add "/usr/local/bin/flint-s3-broker=$B/flint-s3-broker" \
    --label "us.chert.drill-sha=$SHA" \
    --tag "docker.io/dilipdalton/flint-s3-csi:$TAG" --out "$STAGE/images/s3csi.tar" \
    > "$STAGE/images/s3csi.json" || die "derive s3csi"
  python3 "$HERE/oci_derive.py" --base dilipdalton/flint-lean-operator:1.51.0 --platform linux/amd64 \
    --add "/usr/local/bin/flint-lean-operator=$B/flint-lean-operator" \
    --add "/usr/local/bin/flint-lean-gateway=$B/flint-lean-gateway" \
    --label "us.chert.drill-sha=$SHA" \
    --tag "docker.io/dilipdalton/flint-lean-operator:$TAG" --out "$STAGE/images/operator.tar" \
    > "$STAGE/images/operator.json" || die "derive operator"

  helm template flint-s3-csi "$REPO/flint-s3-csi-chart" -n flint-system --include-crds \
    --set node.region="$REGION" \
    --set node.image.tag="$TAG" \
    --set workers.leanImage.tag="$TAG" \
    --set workers.resources.limits.memory=2Gi \
    --set broker.static.secretRef=broker-creds \
    > "$STAGE/charts/s3csi.yaml" || die "render s3csi"
  helm template flint-lean "$REPO/flint-lean-chart" -n flint-system --include-crds \
    --set image.tag="$TAG" \
    --set operatorCredentialsSecret=broker-creds \
    > "$STAGE/charts/lean.yaml" || die "render lean"
  # The names drill.sh waits on, and the tags every pod must run.
  grep -E '^kind: (DaemonSet|Deployment)' -A3 "$STAGE"/charts/*.yaml | grep -E 'name:' >&2
  grep -h -E 'image: ' "$STAGE"/charts/*.yaml | sort -u >&2
  if grep -h -E 'image: .*dilipdalton' "$STAGE"/charts/*.yaml | grep -v "$TAG"; then
    die "a rendered flint image is not $TAG"
  fi
  grep -q "FLINT_S3CSI_LEAN_IMAGE" "$STAGE/charts/s3csi.yaml" && grep -A1 FLINT_S3CSI_LEAN_IMAGE "$STAGE/charts/s3csi.yaml" | grep -q "$TAG" \
    || die "the plugin's lean worker image is not $TAG"
  log "artifacts for $TAG in $STAGE"
}

# ── bucket + policy ───────────────────────────────────────────────────
bucket() {
  admin sts get-caller-identity >&2 || die "trove-admin: aws sso login --profile $ADMIN_PROFILE"
  if ! admin s3api head-bucket --bucket "$BUCKET" 2>/dev/null; then
    admin s3api create-bucket --bucket "$BUCKET" --create-bucket-configuration LocationConstraint="$REGION" >&2 || die "create bucket"
  fi
  admin s3api get-bucket-versioning --bucket "$BUCKET" >&2
  local doc
  doc=$(printf '{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":"s3:*","Resource":["arn:aws:s3:::%s","arn:aws:s3:::%s/*"]}]}' "$BUCKET" "$BUCKET")
  admin iam put-role-policy --role-name "$NODE_ROLE" --policy-name "$POLICY" --policy-document "$doc" >&2 || die "put-role-policy"
  admin iam list-role-policies --role-name "$NODE_ROLE" >&2
  log "bucket $BUCKET ready; policy $POLICY on $NODE_ROLE"
}

# ── stage ─────────────────────────────────────────────────────────────
SCRIPTS=${SCRIPTS:-"drill.sh agent.sh ui.sh shipper.sh podwatch.sh sampler.py hostlegs.sh verify_image.sh"}
stage() {
  local s
  mkdir -p "$STAGE/scripts"
  for s in $SCRIPTS; do
    [ -f "$HERE/$s" ] || die "missing $HERE/$s"
    cp "$HERE/$s" "$STAGE/scripts/$s"
  done
  cp -R "$STAGE/charts" "$STAGE/scripts/"
  aws s3 sync "$STAGE/scripts/" "s3://$BUCKET/_rig/scripts/" --delete --only-show-errors || die "scripts"
  aws s3 sync "$STAGE/bin/" "s3://$BUCKET/_rig/bin/" --delete --only-show-errors || die "bin"
  aws s3 cp "$STAGE/bin.SHA256SUMS" "s3://$BUCKET/_rig/bin.SHA256SUMS" --only-show-errors || die "sums"
  aws s3 sync "$STAGE/images/" "s3://$BUCKET/_rig/images/" --only-show-errors || die "images"
  aws s3 ls --recursive "s3://$BUCKET/_rig/" | awk '{s+=$3; n++} END {print n " objects, " s/1048576 " MiB"}' >&2
}

# ── provision ─────────────────────────────────────────────────────────
provision() {
  [ "${1:-}" = --yes ] || die "provisioning needs --yes (the user approves it, not the script)"
  curl -sk -m 5 -o /dev/null -w '%{http_code}' -H 'Authorization: Bearer trove-dummy-token' \
    https://localhost:8080/api/v1/projects | grep -q 200 || die "trove is not answering on :8080"
  (cd /Users/ddalton/github/trove && env TROVE_AWS_DEFAULT_INSTANCE_TYPE=i4i.large \
      fish scripts/aws-live-allspot.fish create "$CLUSTER" 3)
}

# ── nodes + SSM ───────────────────────────────────────────────────────
nodes() {
  aws ec2 describe-instances \
    --filters "Name=tag:trove:cluster,Values=$CLUSTER" "Name=instance-state-name,Values=running" \
    --query 'Reservations[].Instances[].[InstanceId,Tags[?Key==`trove:node-name`]|[0].Value,InstanceType,InstanceLifecycle,PrivateIpAddress]' \
    --output text | sort -k2 > "$STAGE/nodes.tsv"
  cat "$STAGE/nodes.tsv" >&2
  [ -s "$STAGE/nodes.tsv" ] || die "no running instances tagged trove:cluster=$CLUSTER"
}
targets() { # <id|cp|all|workers>
  case $1 in
    all) awk '{print $1}' "$STAGE/nodes.tsv" ;;
    cp) grep -E -- '-cp-' "$STAGE/nodes.tsv" | awk '{print $1}' | head -1 ;;
    workers) grep -v -E -- '-cp-' "$STAGE/nodes.tsv" | awk '{print $1}' ;;
    *) echo "$1" ;;
  esac
}
ssm() { # <target> <command…>
  local tgt=$1; shift
  local cmd="$*" id cid st
  for id in $(targets "$tgt"); do
    cid=$(aws ssm send-command --instance-ids "$id" --document-name AWS-RunShellScript \
          --parameters "$(python3 -c 'import json,sys; print(json.dumps({"commands": [sys.argv[1]], "executionTimeout": ["7200"]}))' "$cmd")" \
          --output-s3-bucket-name "$BUCKET" --output-s3-key-prefix "_rig/ssm" \
          --timeout-seconds 600 --query Command.CommandId --output text) || die "send-command $id"
    while :; do
      st=$(aws ssm get-command-invocation --command-id "$cid" --instance-id "$id" --query Status --output text 2>/dev/null)
      case "$st" in Success|Failed|Cancelled|TimedOut) break ;; esac
      sleep 3
    done
    echo "── $id ($st)" >&2
    aws s3 cp "s3://$BUCKET/_rig/ssm/$cid/$id/awsrunShellScript/0.awsrunShellScript/stdout" - 2>/dev/null
    aws s3 cp "s3://$BUCKET/_rig/ssm/$cid/$id/awsrunShellScript/0.awsrunShellScript/stderr" - >&2 2>/dev/null
    [ "$st" = Success ] || return 1
  done
}

node_name() { awk -v id="$1" '$1==id {print $2}' "$STAGE/nodes.tsv"; }

prep() {
  local id img
  for id in $(targets all); do
    log "prep $id ($(node_name "$id"))"
    ssm "$id" "set -e; mkdir -p /mnt/nvme/rig && cd /mnt/nvme/rig && \
      aws s3 sync s3://$BUCKET/_rig/scripts/ . --only-show-errors && chmod +x *.sh *.py && \
      for img in worker-lean s3csi operator; do aws s3 cp s3://$BUCKET/_rig/images/\$img.tar - | ctr -n k8s.io images import --platform linux/amd64 - ; done && \
      ctr -n k8s.io images ls -q | grep $TAG" || die "image import on $id"
    # Content, not tags: the binary inside each image equals the one built here.
    ssm "$id" "cd /mnt/nvme/rig && \
      ./verify_image.sh docker.io/dilipdalton/flint-s3-worker-lean:$TAG /usr/local/bin/flint-sync $(awk '$2=="flint-sync"{print $1}' "$STAGE/bin.SHA256SUMS") && \
      ./verify_image.sh docker.io/dilipdalton/flint-s3-csi:$TAG /usr/local/bin/flint-s3-csi-node $(awk '$2=="flint-s3-csi-node"{print $1}' "$STAGE/bin.SHA256SUMS") && \
      ./verify_image.sh docker.io/dilipdalton/flint-lean-operator:$TAG /usr/local/bin/flint-lean-operator $(awk '$2=="flint-lean-operator"{print $1}' "$STAGE/bin.SHA256SUMS")" \
      || die "image content check failed on $id"
    ssm "$id" "cd /mnt/nvme/rig && BUCKET=$BUCKET NODE=$(node_name "$id") EVID=/mnt/nvme/evidence ./shipper.sh start && ./shipper.sh status" \
      || die "shipper on $id"
  done
  ssm cp "cd /mnt/nvme/rig && BUCKET=$BUCKET ./drill.sh fetch && BUCKET=$BUCKET EVID=/mnt/nvme/evidence ./podwatch.sh start" || die "CP fetch/podwatch"
}

cpdo() { ssm cp "cd /mnt/nvme/rig && BUCKET=$BUCKET ./drill.sh $*"; }

leg() { # <leg>
  local leg=$1 rc
  ssm cp "cd /mnt/nvme/rig && rm -f leg-$leg.rc && (BUCKET=$BUCKET setsid nohup bash -c './drill.sh leg $leg > leg-$leg.log 2>&1; echo \$? > leg-$leg.rc' >/dev/null 2>&1 &) && echo started" \
    || die "start leg $leg"
  while :; do
    sleep 60
    rc=$(ssm cp "cat /mnt/nvme/rig/leg-$leg.rc 2>/dev/null; tail -3 /mnt/nvme/rig/leg-$leg.log" 2>/dev/null)
    echo "$rc" | tail -3 >&2
    if echo "$rc" | head -1 | grep -q -E '^[0-9]+$'; then
      [ "$(echo "$rc" | head -1)" = 0 ] && { log "leg $leg run done"; return 0; }
      log "leg $leg FAILED on the CP (rc $(echo "$rc" | head -1))"; return 1
    fi
  done
}

# ── verdict ───────────────────────────────────────────────────────────
leg_oracles() {
  case $1 in
    A1) echo "O1,O2,O3,O5" ;;
    A2) echo "O1,O2,O3,O4,O5" ;;
    A3) echo "O1,O2,O3,O4,O5" ;;
    A4) echo "O1,O2,O3,O5" ;;
    *) die "no oracle set for $1" ;;
  esac
}
verdict() { # <leg>
  local leg=$1 dir=$STAGE/judge/$1 want got
  ssm all "cd /mnt/nvme/rig && BUCKET=$BUCKET NODE=\$(hostname) ./shipper.sh flush" >/dev/null || log "a shipper flush failed"
  ssm cp "cd /mnt/nvme/rig && BUCKET=$BUCKET EVID=/mnt/nvme/evidence ./podwatch.sh flush" >/dev/null || log "podwatch flush failed"
  rm -rf "$dir" && mkdir -p "$dir"
  aws s3 cp "s3://$BUCKET/_rig/collect/$leg.tgz" "$dir/collect.tgz" --only-show-errors || die "pull collect"
  want=$(aws s3 cp "s3://$BUCKET/_rig/collect/$leg.sha256" -)
  got=$(shasum -a 256 "$dir/collect.tgz" | awk '{print $1}')
  [ "$want" = "$got" ] || die "collect tarball checksum: $want vs $got"
  tar xzf "$dir/collect.tgz" -C "$dir" || die "untar"
  aws s3 sync "s3://$BUCKET/_rig/evidence/" "$STAGE/evidence/" --only-show-errors || die "pull evidence"
  python3 "$HERE/extract_traces.py" --evidence "$STAGE/evidence" --collect "$dir/$leg" || die "extract traces"
  local extra=()
  [ "$(leg_faults_of "$leg")" = 1 ] && extra+=(--faults-declared)
  python3 "$HERE/oracle.py" "$dir/$leg" --oracles "$(leg_oracles "$leg")" "${extra[@]}" > "$dir/verdict.json"
  local rc=$?
  python3 -c 'import json,sys; v=json.load(open(sys.argv[1])); print("VERDICT", v["leg"], "PASS" if v["pass"] else "FAIL"); [print(" ", k, "pass" if o["pass"] else "FAIL", o.get("reasons", "")) for k, o in v["oracles"].items()]' "$dir/verdict.json"
  aws s3 cp "$dir/verdict.json" "s3://$BUCKET/_rig/verdicts/$leg.json" --only-show-errors
  return $rc
}
leg_faults_of() { case $1 in A4) echo 1;; *) echo 0;; esac; }

status() {
  ssm all "echo \$(hostname) \$(df -h /mnt/nvme | awk 'NR==2{print \$4\" free\"}') \$(cd /mnt/nvme/rig 2>/dev/null && ./shipper.sh status 2>&1 | tr '\n' ' ')"
}

cmd=${1:-}; shift || true
mkdir -p "$STAGE"
case "$cmd" in
  artifacts) artifacts ;;
  bucket) bucket ;;
  stage) stage ;;
  provision) provision "$@" ;;
  nodes) nodes ;;
  ssm) ssm "$@" ;;
  prep) prep ;;
  cp) cpdo "$@" ;;
  leg) leg "${1:?leg}" ;;
  verdict) verdict "${1:?leg}" ;;
  status) status ;;
  *) sed -n '2,22p' "$0"; exit 2 ;;
esac
