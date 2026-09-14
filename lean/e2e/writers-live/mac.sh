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
#   mac.sh evidence                shipper on every node + podwatch on the CP, no image import
#   mac.sh verdict-idle A1 <N>     A5: O6 over A1's writers in the idle window, N requests/tick; seq +1 control
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
  # cp --recursive, never sync: sync skips a file whose size matches, and a
  # rebuilt binary, a re-rendered chart (a tag of the same length) or an
  # image tar (padded records) all can — each has shipped the OLD build.
  aws s3 cp --recursive "$STAGE/scripts/" "s3://$BUCKET/_rig/scripts/" --only-show-errors || die "scripts"
  aws s3 cp --recursive "$STAGE/bin/" "s3://$BUCKET/_rig/bin/" --only-show-errors || die "bin"
  echo "$TAG" | aws s3 cp - "s3://$BUCKET/_rig/TAG" || die "tag"
  aws s3 cp "$STAGE/bin.SHA256SUMS" "s3://$BUCKET/_rig/bin.SHA256SUMS" --only-show-errors || die "sums"
  # cp, never sync: two image tars of different builds can have the SAME
  # size (tar pads to whole records), and sync skipped the new operator
  # tar that way — a node imported the previous build under its old tag.
  (cd "$STAGE/images" && shasum -a 256 *.tar > "$STAGE/images.SHA256SUMS")
  for f in "$STAGE"/images/*.tar; do
    aws s3 cp "$f" "s3://$BUCKET/_rig/images/$(basename "$f")" --only-show-errors || die "image $f"
  done
  aws s3 cp "$STAGE/images.SHA256SUMS" "s3://$BUCKET/_rig/images.SHA256SUMS" --only-show-errors || die "image sums"
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
      aws s3 cp --recursive s3://$BUCKET/_rig/scripts/ . --only-show-errors && chmod +x *.sh *.py && \
      for img in worker-lean s3csi operator; do aws s3 cp s3://$BUCKET/_rig/images/\$img.tar - | ctr -n k8s.io images import --platform linux/amd64 - ; done && \
      ctr -n k8s.io images ls -q | grep $TAG" || die "image import on $id"
    # Content, not tags: the binary inside each image equals the one built here.
    ssm "$id" "cd /mnt/nvme/rig && \
      ./verify_image.sh docker.io/dilipdalton/flint-s3-worker-lean:$TAG /usr/local/bin/flint-sync $(awk '$2=="flint-sync"{print $1}' "$STAGE/bin.SHA256SUMS") && \
      ./verify_image.sh docker.io/dilipdalton/flint-s3-csi:$TAG /usr/local/bin/flint-s3-csi-node $(awk '$2=="flint-s3-csi-node"{print $1}' "$STAGE/bin.SHA256SUMS") && \
      ./verify_image.sh docker.io/dilipdalton/flint-lean-operator:$TAG /usr/local/bin/flint-lean-operator $(awk '$2=="flint-lean-operator"{print $1}' "$STAGE/bin.SHA256SUMS")" \
      || die "image content check failed on $id"
  done
  ssm cp "cd /mnt/nvme/rig && BUCKET=$BUCKET ./drill.sh fetch" || die "CP fetch"
  evidence
}

# The evidence loops alone, on a fleet whose images are already imported and
# verified: fresh scripts (cp, never sync — same-size files are skipped), the
# three images still present (kubelet GC removes an unused one), then the
# shipper on every node and podwatch on the CP. The shipper records NODE at
# its first start; podwatch keeps its own EVID/STATE defaults.
evidence() {
  local id
  for id in $(targets all); do
    log "evidence $id ($(node_name "$id"))"
    ssm "$id" "set -e; mkdir -p /mnt/nvme/rig && cd /mnt/nvme/rig && \
      aws s3 cp --recursive s3://$BUCKET/_rig/scripts/ . --only-show-errors && chmod +x *.sh *.py && \
      test \$(ctr -n k8s.io images ls -q | grep -c ':$TAG\$') -ge 3 && \
      BUCKET=$BUCKET NODE=$(node_name "$id") ./shipper.sh start && ./shipper.sh status" \
      || die "evidence on $id"
  done
  ssm cp "cd /mnt/nvme/rig && BUCKET=$BUCKET ./podwatch.sh start && ./podwatch.sh status" || die "CP podwatch"
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
  ssm all "cd /mnt/nvme/rig && BUCKET=$BUCKET ./shipper.sh flush" >/dev/null || log "a shipper flush failed"
  ssm cp "cd /mnt/nvme/rig && BUCKET=$BUCKET ./podwatch.sh flush" >/dev/null || log "podwatch flush failed"
  rm -rf "$dir" && mkdir -p "$dir"
  aws s3 cp "s3://$BUCKET/_rig/collect/$leg.tgz" "$dir/collect.tgz" --only-show-errors || die "pull collect"
  want=$(aws s3 cp "s3://$BUCKET/_rig/collect/$leg.sha256" -)
  got=$(shasum -a 256 "$dir/collect.tgz" | awk '{print $1}')
  [ "$want" = "$got" ] || die "collect tarball checksum: $want vs $got"
  tar xzf "$dir/collect.tgz" -C "$dir" || die "untar"
  aws s3 sync "s3://$BUCKET/_rig/evidence/" "$STAGE/evidence/" --exact-timestamps --only-show-errors || die "pull evidence"
  # Agent pods are `agents-N` in EVERY leg's namespace, and a leg's fleet
  # outlives it (A5 idles A1's): without the namespace and the leg's own
  # window, another leg's (or an aborted attempt's) workers merge into this
  # leg's traces. The window opens at the prefix's stamp (taken before the
  # namespace is applied) and closes at collect.
  local from_ms to_ms
  from_ms=$(python3 -c 'import sys,datetime; s=open(sys.argv[1]).read().strip().rsplit("-",1)[1]; print(int(datetime.datetime.strptime(s,"%Y%m%d%H%M%S").replace(tzinfo=datetime.timezone.utc).timestamp()*1000))' "$dir/$leg/prefix") || die "prefix stamp"
  to_ms=$(tr -d '[:space:]' < "$dir/$leg/t_end_ms") || die "t_end_ms"
  python3 "$HERE/extract_traces.py" --evidence "$STAGE/evidence" --collect "$dir/$leg" \
    --tenant-ns "^wl-$(echo "$leg" | tr 'A-Z' 'a-z')\$" --from-ms "$from_ms" --to-ms "$to_ms" || die "extract traces"
  local extra=()
  [ "$(leg_faults_of "$leg")" = 1 ] && extra+=(--faults-declared)
  # A fault leg whose faults did not land judges nothing about faults: the
  # first A4 passed with both worker deletions refused by admission.
  if [ "$(leg_faults_of "$leg")" = 1 ]; then
    local void="" dels kills
    [ -f "$dir/$leg/faults.void" ] && void="faults.void: $(tr '\n' ';' < "$dir/$leg/faults.void")"
    dels=$(grep -c '"fault":"delete-worker"' "$dir/$leg/faults.jsonl" 2>/dev/null || true)
    kills=$(grep -c '"fault":"kill-9-syncer"' "$dir/$leg/faults.jsonl" 2>/dev/null || true)
    [ "${dels:-0}" -ge 1 ] && [ "${kills:-0}" -ge 1 ] || void="$void effective faults: ${dels:-0} deletions, ${kills:-0} kills"
    if [ -n "$void" ]; then
      log "VERDICT $leg VOID — $void"
      echo "{\"leg\":\"$leg\",\"void\":true,\"why\":\"$void\"}" | aws s3 cp - "s3://$BUCKET/_rig/verdicts/$leg.void.json" --only-show-errors
      return 2
    fi
    log "faults that landed: $dels worker deletions, $kills syncer kills"
  fi
  python3 "$HERE/oracle.py" "$dir/$leg" --oracles "$(leg_oracles "$leg")" --require-extract-report ${extra[@]+"${extra[@]}"} > "$dir/verdict.json"
  local rc=$?
  python3 -c 'import json,sys; v=json.load(open(sys.argv[1])); print("VERDICT", v["leg"], "PASS" if v["pass"] else "FAIL"); [print(" ", k, "skipped" if o["pass"] is None else "pass" if o["pass"] else "FAIL", o.get("details", {}).get("reasons", "")) for k, o in v["oracles"].items()]' "$dir/verdict.json"
  aws s3 cp "$dir/verdict.json" "s3://$BUCKET/_rig/verdicts/$leg.json" --only-show-errors
  return $rc
}
leg_faults_of() { case $1 in A4) echo 1;; *) echo 0;; esac; }

# A5 has no collect of its own: it idles <fleet-leg>'s paused writers. Its
# traces are that fleet's (namespace wl-<fleet>) over A5's window; O6 judges
# the idle window against <baseline> requests per tick (measured, never
# assumed), and the control is the one edit moving seq by exactly one.
verdict_idle() { # <fleet-leg> <baseline-requests-per-tick>
  local fleet=$1 baseline=$2 dir=$STAGE/judge/A5 t
  local from to end edit
  ssm all "cd /mnt/nvme/rig && BUCKET=$BUCKET ./shipper.sh flush" >/dev/null || log "a shipper flush failed"
  ssm cp "cd /mnt/nvme/rig && BUCKET=$BUCKET ./podwatch.sh flush" >/dev/null || log "podwatch flush failed"
  rm -rf "$dir" && mkdir -p "$dir/A5"
  [ -f "$STAGE/judge/$fleet/$fleet/meta.json" ] || die "judge $fleet first (its meta.json names the fleet)"
  python3 -c 'import json,sys; m=json.load(open(sys.argv[1])); m["leg"]="A5"; json.dump(m, open(sys.argv[2],"w"), indent=1)' \
    "$STAGE/judge/$fleet/$fleet/meta.json" "$dir/A5/meta.json" || die "meta"
  cp "$STAGE/judge/$fleet/$fleet/agent_nodes.json" "$dir/A5/" 2>/dev/null || true
  for t in t_idle_from_ms t_idle_to_ms t_end_ms; do
    ssm cp "cat /mnt/nvme/collect/A5/$t" 2>/dev/null | grep -E '^[0-9]{13}$' > "$dir/$t" || die "A5 $t (has A5 finished?)"
  done
  from=$(cat "$dir/t_idle_from_ms"); to=$(cat "$dir/t_idle_to_ms"); end=$(cat "$dir/t_end_ms")
  edit=$(aws s3 cp "s3://$BUCKET/_rig/collect/A5-edit.json" -) || die "A5 edit.json"
  echo "$edit" > "$dir/edit.json"
  aws s3 sync "s3://$BUCKET/_rig/evidence/" "$STAGE/evidence/" --exact-timestamps --only-show-errors || die "pull evidence"
  # From one floor-minute before the window, so O6 has each writer's counter before its first idle tick.
  python3 "$HERE/extract_traces.py" --evidence "$STAGE/evidence" --collect "$dir/A5" \
    --tenant-ns "^wl-$(echo "$fleet" | tr 'A-Z' 'a-z')\$" --from-ms $((from - 60000)) --to-ms "$end" || die "extract traces"
  python3 "$HERE/oracle.py" "$dir/A5" --oracles O6 --require-extract-report \
    --idle-from "$from" --idle-to "$to" --request-baseline "$baseline" > "$dir/verdict.json"
  local rc=$?
  python3 -c 'import json,sys; v=json.load(open(sys.argv[1])); e=json.load(open(sys.argv[2])); ok=e["seq_after"]-e["seq_before_edit"]==1
d=v["oracles"]["O6"]; print("VERDICT A5", "PASS" if v["pass"] and ok else "FAIL"); print("  O6", "pass" if d["pass"] else "FAIL", d["details"]["reasons"]); print("  control: seq", e["seq_before_edit"], "->", e["seq_after"], "pass" if ok else "FAIL (want exactly +1)")
sys.exit(0 if v["pass"] and ok else 1)' "$dir/verdict.json" "$dir/edit.json" || rc=1
  aws s3 cp "$dir/verdict.json" "s3://$BUCKET/_rig/verdicts/A5.json" --only-show-errors
  return $rc
}

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
  evidence) evidence ;;
  cp) cpdo "$@" ;;
  leg) leg "${1:?leg}" ;;
  verdict) verdict "${1:?leg}" ;;
  verdict-idle) verdict_idle "${1:?fleet leg}" "${2:?baseline requests per tick}" ;;
  status) status ;;
  *) sed -n '2,22p' "$0"; exit 2 ;;
esac
