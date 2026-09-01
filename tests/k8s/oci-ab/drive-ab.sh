#!/bin/bash
# oci-ab drive script v2 — phase-1 five-arm A/B (§9.4), arms A1/A3/A5.
# Host-side; drives the cluster via kubectl (trove WG tunnel) + SSM.
#
# Env: KC (kubeconfig), CLUSTER (trove cluster name, for EC2 Name tags),
#      BUCKET, S3_KEY_ID, S3_SECRET, REPS (default 5),
#      AWS_PROFILE=rolesanywhere, IMG (default python:3.12).
# Subcommands: setup-cluster | setup-registries | push-image | setup-client |
#              stage-blob | preflight | run | warm-leg | broken-lazy-leg |
#              score | teardown-check
#
# ── WHY v2 ───────────────────────────────────────────────────────────────
# v1 could not produce a trustworthy number. Four defects were fatal and the
# rest were guards the README promises but the code only *recorded*:
#   1. arm() piped SSM stdout into `json.load` -> `run` emitted invalid JSON.
#   2. node_iid read .spec.providerID, which is EMPTY on trove (no
#      cloud-controller-manager). Every SSM call addressed "".
#   3. SSM commands were interpolated into the shorthand `commands=[".."]`
#      form while containing quotes, $ and backslashes.
#   4. ssm_run returned StandardOutputContent regardless of Status, so a
#      FAILED pull was recorded as a very FAST arm. Failure looked like a win.
#   5. The timer wrapped SSM submit + a 2s-granularity poll loop, on a signal
#      whose lazy arm is ~1s. Quantization alone could manufacture the result.
# v2 measures ON the node, checks status, and every guard below can VOID a
# rep. A void is not an error: it is the rig refusing to quote a number it
# cannot stand behind. Guards read STATE, never exit codes.
#
# Anti-vacuity: `rig-selftest.sh` drives this file against fake kubectl/aws
# and asserts every guard actually fires on a violation. A guard that has
# never been seen to fail is not a guard.
set -uo pipefail

KC=${KC:?kubeconfig path}
K="kubectl --kubeconfig $KC"
IMG=${IMG:-python:3.12}
REPS=${REPS:-5}
CLUSTER=${CLUSTER:-}
HERE="$(cd "$(dirname "$0")" && pwd)"
PYEXEC='import json,ssl,sqlite3,decimal,email,http.client;print("READY")'

# Guard thresholds. Named, so a void reason points at the knob that voided it.
MAX_LOADAVG=${MAX_LOADAVG:-1.5}     # per-vCPU; saturation compresses ratios to 1.0
DS_SETTLE_TRIES=${DS_SETTLE_TRIES:-30}
MIN_VALID_REPS=${MIN_VALID_REPS:-3} # below this, `score` refuses to report

k() { $K "$@"; }
warn() { echo "$*" >&2; }

# ── node identity ────────────────────────────────────────────────────────
# trove clusters have no cloud-controller-manager, so .spec.providerID is
# empty and node_iid used to return "". Map through the EC2 Name tag, and
# FAIL LOUDLY rather than address the empty string.
client_node() { k get nodes -l oci-ab/role=client -o jsonpath='{.items[0].metadata.name}'; }
node_iid() {
  local n=$1 iid
  [ -n "$CLUSTER" ] || { warn "FATAL: CLUSTER unset — needed for the EC2 Name tag (trove/<cluster>/<node>)"; return 1; }
  iid=$(aws ec2 describe-instances \
        --filters "Name=tag:Name,Values=trove/$CLUSTER/$n" Name=instance-state-name,Values=running \
        --query 'Reservations[0].Instances[0].InstanceId' --output text 2>/dev/null)
  case "${iid:-None}" in None|"") warn "FATAL: no running instance tagged trove/$CLUSTER/$n"; return 1;; esac
  echo "$iid"
}

# ── SSM: b64-shipped (no shorthand parsing hazards), status-checked ──────
# Returns 0 only on Status=Success. stdout is the command's stdout ONLY;
# every diagnostic goes to stderr, so callers can parse stdout safely.
ssm_run() { # $1=instance-id, $2=command string
  local iid=$1 cmd=$2 b64 cid st i
  b64=$(printf '%s' "$cmd" | base64 | tr -d '\n')
  cid=$(aws ssm send-command --instance-ids "$iid" --document-name AWS-RunShellScript \
        --timeout-seconds 900 --parameters commands="echo $b64 | base64 -d | bash" \
        --query Command.CommandId --output text 2>/dev/null)
  [ -n "${cid:-}" ] && [ "$cid" != None ] || { warn "ssm: send-command failed on $iid"; return 1; }
  st=Pending
  for i in $(seq 1 150); do
    st=$(aws ssm get-command-invocation --command-id "$cid" --instance-id "$iid" \
         --query Status --output text 2>/dev/null || echo Pending)
    case $st in Success|Failed|Cancelled|TimedOut) break;; *) sleep 4;; esac
  done
  aws ssm get-command-invocation --command-id "$cid" --instance-id "$iid" \
    --query StandardOutputContent --output text 2>/dev/null
  if [ "$st" != Success ]; then
    warn "ssm: status=$st on $iid"
    aws ssm get-command-invocation --command-id "$cid" --instance-id "$iid" \
      --query StandardErrorContent --output text 2>/dev/null | tail -8 >&2
    return 1
  fi
}

reg_ip() { k get svc "$1" -o jsonpath='{.spec.clusterIP}'; }

# ── registry request accounting ─────────────────────────────────────────
# v1 counted ALL log lines with no time window: O(n^2) re-reads, and a log
# rotation makes the count go DOWN, so a delta could be negative and an
# attribution violation could hide as a small number. Count v2 API requests
# inside an explicit window instead.
reg_reqs_since() { # $1=deploy $2=RFC3339 timestamp
  k logs "deploy/$1" --since-time="$2" 2>/dev/null | grep -cE '"(GET|HEAD|POST|PUT|PATCH)[^"]*/v2/' || true
}
now_rfc3339() { date -u +%Y-%m-%dT%H:%M:%SZ; }

# ── guards ───────────────────────────────────────────────────────────────
# Each returns 0 (clear) or prints a void reason and returns 1.

# G-CLOCK: the host clock must produce integer nanoseconds. BSD date without
# %N support yields a literal "N" and every arithmetic result downstream is
# garbage. Host time is only a sanity bound in v2, but a broken bound must
# still announce itself.
guard_clock() {
  local t; t=$(date +%s%N)
  case "$t" in ''|*[!0-9]*) echo "G-CLOCK:date-+%s%N-not-numeric($t)"; return 1;; esac
  [ ${#t} -ge 18 ] || { echo "G-CLOCK:nanosecond-field-missing($t)"; return 1; }
}

# G-SETTLE: the DS fleet must be fully registered with no recent rejections.
# This is the confound that voided the first runbx GREEN — a push ~30 s after
# a rolling image swap, while DSes were still re-registering, is two
# variables. Never measure a fleet in motion.
guard_settle() {
  local want got rej i
  want=$(k -n flint-system get deploy/flint-pnfs-mds -o jsonpath='{.metadata.annotations.flint\.io/ds-count}' 2>/dev/null)
  want=${want:-3}
  for i in $(seq 1 "$DS_SETTLE_TRIES"); do
    got=$(k -n flint-system get pods -l app=flint-pnfs-ds \
          -o jsonpath='{range .items[*]}{.status.phase}{"\n"}{end}' 2>/dev/null | grep -c Running || true)
    rej=$(k -n flint-system logs deploy/flint-pnfs-mds --since=60s 2>/dev/null | grep -c "rejected" || true)
    [ "${got:-0}" -eq "$want" ] && [ "${rej:-1}" -eq 0 ] && return 0
    [ "$i" -lt "$DS_SETTLE_TRIES" ] && sleep 10
  done
  echo "G-SETTLE:ds-active=${got:-0}/$want,recent-rejections=${rej:-?}"
  return 1
}

# G-IDLE: saturation compresses every ratio toward 1.0, which is the one
# failure that makes a null result look like a real one. Checked per ARM,
# not once per rep — a previous arm can leave the node loaded.
guard_idle() { # $1=iid
  local la
  la=$(ssm_run "$1" 'cut -d" " -f1 /proc/loadavg; nproc' 2>/dev/null | tr '\n' ' ')
  set -- $la
  local load=${1:-} cpus=${2:-1}
  case "${load:-x}" in ''|*[!0-9.]*) echo "G-IDLE:unreadable-loadavg"; return 1;; esac
  awk -v l="$load" -v c="$cpus" -v m="$MAX_LOADAVG" 'BEGIN{exit !((l/c) > m)}' \
    && { echo "G-IDLE:loadavg=$load/${cpus}cpu>$MAX_LOADAVG"; return 1; }
  echo "$load"
}

# G-COLD: verify coldness by STATE. v1's cold() printed "cold-ok"
# unconditionally, so a failed prune was measured as a cold pull — the exact
# shape of "a check that can only confirm".
cold_and_verify() { # $1=iid $2=registry
  local out
  out=$(ssm_run "$1" "
    nerdctl rm -f \$(nerdctl ps -aq) 2>/dev/null
    nerdctl rmi -f $2/$IMG 2>/dev/null
    nerdctl system prune -af >/dev/null 2>&1
    systemctl stop soci-snapshotter
    rm -rf /var/lib/soci-snapshotter-grpc
    systemctl start soci-snapshotter
    sleep 2
    echo \"#RIG images=\$(nerdctl image ls -q | wc -l) socistate=\$([ -d /var/lib/soci-snapshotter-grpc ] && echo present || echo absent) soci=\$(systemctl is-active soci-snapshotter)\"
  ") || { echo "G-COLD:ssm-failed"; return 1; }
  local imgs; imgs=$(echo "$out" | sed -n 's/.*images=\([0-9]*\).*/\1/p')
  local soci; soci=$(echo "$out" | sed -n 's/.*soci=\([a-z]*\).*/\1/p')
  case "${imgs:-x}" in ''|*[!0-9]*) echo "G-COLD:no-image-count-in-output"; return 1;; esac
  [ "$imgs" -eq 0 ] || { echo "G-COLD:$imgs-images-survived-prune"; return 1; }
  [ "$soci" = active ] || { echo "G-COLD:soci-snapshotter=$soci"; return 1; }
}

# ── one measured arm ─────────────────────────────────────────────────────
# Emits exactly ONE json object on stdout. Everything else goes to stderr.
# Timing is taken ON THE NODE: the host-side stopwatch in v1 wrapped SSM
# submit plus a 2 s poll loop, which is larger than the lazy arm's entire
# signal.
measure_arm() { # $1=arm $2=snapshotter $3=registry $4=iid $5=rep $6=expected_digest $7=FLINT|S3
  local armn=$1 snap=$2 reg=$3 iid=$4 rep=$5 want_dig=$6 which=$7
  local void="" load="" t_host0 t_host1 out ready pull prc rrc dig f0 s0 f1 s1 fd sd since

  t_host0=$(date +%s%N)
  if ! void=$(cold_and_verify "$iid" "$reg"); then emit_void "$armn" "$rep" "$void"; return; fi

  if ! load=$(guard_idle "$iid"); then emit_void "$armn" "$rep" "$load"; return; fi

  since=$(now_rfc3339)
  f0=$(reg_reqs_since registry-flint "$since"); s0=$(reg_reqs_since registry-s3 "$since")

  # node-side stopwatch + digest of what was ACTUALLY pulled
  out=$(ssm_run "$iid" "
    set -u
    t0=\$(date +%s%N)
    nerdctl --snapshotter $snap pull --quiet --hosts-dir /etc/containerd/certs.d $reg/$IMG >/dev/null 2>&1; prc=\$?
    t1=\$(date +%s%N)
    nerdctl --snapshotter $snap run --rm $reg/$IMG python3 -c '$PYEXEC' >/dev/null 2>&1; rrc=\$?
    t2=\$(date +%s%N)
    dig=\$(nerdctl image inspect --format '{{index .RepoDigests 0}}' $reg/$IMG 2>/dev/null | sed 's/.*@//')
    echo \"#RIG pull_ms=\$(( (t1-t0)/1000000 )) ready_ms=\$(( (t2-t0)/1000000 )) prc=\$prc rrc=\$rrc digest=\${dig:-none}\"
  ") || { emit_void "$armn" "$rep" "G-SSM:command-did-not-succeed"; return; }
  t_host1=$(date +%s%N)

  ready=$(echo "$out" | sed -n 's/.*ready_ms=\([0-9]*\).*/\1/p')
  pull=$(echo "$out" | sed -n 's/.*pull_ms=\([0-9]*\).*/\1/p')
  prc=$(echo "$out" | sed -n 's/.*prc=\([0-9]*\).*/\1/p')
  rrc=$(echo "$out" | sed -n 's/.*rrc=\([0-9]*\).*/\1/p')
  dig=$(echo "$out" | sed -n 's/.*digest=\([^ ]*\).*/\1/p')

  case "${ready:-x}" in ''|*[!0-9]*) emit_void "$armn" "$rep" "G-PARSE:no-#RIG-line-in-node-output"; return;; esac
  [ "${prc:-1}" -eq 0 ] || { emit_void "$armn" "$rep" "G-PULL:pull-rc=$prc"; return; }
  [ "${rrc:-1}" -eq 0 ] || { emit_void "$armn" "$rep" "G-RUN:run-rc=$rrc"; return; }

  # G-INTEG: this campaign's own primary finding is that the substrate can
  # serve corrupt bytes with NFS4_OK. A perf number taken over corrupt bytes
  # is not a slow result, it is not a result. Digest-gate every rep.
  if [ -n "$want_dig" ] && [ "$want_dig" != none ] && [ "$dig" != "$want_dig" ]; then
    emit_void "$armn" "$rep" "G-INTEG:pulled=$dig,pushed=$want_dig"; return
  fi

  f1=$(reg_reqs_since registry-flint "$since"); s1=$(reg_reqs_since registry-s3 "$since")
  fd=$(( f1 - f0 )); sd=$(( s1 - s0 ))

  # G-ATTR: the README's own VOID rule, which v1 recorded and ignored. The
  # arm's own backend must have served requests; the OTHER backend must have
  # served none. Zero on your own backend means the arm never went remote —
  # a warm hit measured as cold.
  # Which backend this arm is SUPPOSED to use is an input, not something to
  # re-derive by substring-matching an IP that could be empty (an empty
  # pattern matches everything, and the arm would grade itself against the
  # wrong backend — a guard that mislabels is worse than no guard).
  local own other othername
  case "$which" in
    FLINT) own=$fd; other=$sd; othername=registry-s3;;
    S3)    own=$sd; other=$fd; othername=registry-flint;;
    *)     emit_void "$armn" "$rep" "G-ATTR:unknown-backend-label($which)"; return;;
  esac
  [ "$own" -gt 0 ] || { emit_void "$armn" "$rep" "G-ATTR:own-backend-served-0-requests"; return; }
  [ "$other" -eq 0 ] || { emit_void "$armn" "$rep" "G-ATTR:$othername-served-$other-requests"; return; }

  printf '{"rep":%s,"arm":"%s","valid":true,"ready_ms":%s,"pull_ms":%s,"loadavg":%s,"reqs_own":%s,"reqs_other":%s,"digest":"%s","host_ms":%s}\n' \
    "$rep" "$armn" "$ready" "${pull:-0}" "$load" "$own" "$other" "$dig" "$(( (t_host1-t_host0)/1000000 ))"
}

emit_void() { # $1=arm $2=rep $3=reason
  warn "VOID rep$2 $1: $3"
  printf '{"rep":%s,"arm":"%s","valid":false,"void":"%s"}\n' "$2" "$1" "$3"
}

# Arm order rotates per rep. A fixed order aliases order effects (registry
# page cache, connection reuse) onto arm identity; interleaving is only
# interleaving if the position changes.
arm_order() { # $1=rep -> rotation of the three specs
  local specs=("A1 overlayfs FLINT" "A3 soci S3" "A5 soci FLINT")
  local n=${#specs[@]} i off=$(( ($1 - 1) % 3 ))
  for i in 0 1 2; do echo "${specs[$(( (i + off) % n ))]}"; done
}

# ── G-WIDTH: the substrate stripe-width gate ─────────────────────────────
# flint-29's stripe-width-gate.py is a THREE-state check and the third state
# is the point: exit 0 PASS, 1 FAIL, 2 INCONCLUSIVE. The 1.43.0 defect only
# manifests on BOUNDED LAYOUTGETs, so a log containing only whole-file grants
# cannot exonerate anything, and at INFO the lines do not exist at all. Both
# of those are blindness, not health — treating exit != 1 as a pass would
# reproduce this campaign's signature failure ("the check passed because the
# question was never asked") on the one gate built to prevent it.
# Sibling of G-SETTLE: never let an unasked question read as a clean answer.
# The MDS log must be STREAMED while the workload runs, never fetched after
# it. flint-29 measured this on runby: at debug level a 600 MB push overruns
# the container log and rotates the LAYOUTGET lines away in under a minute,
# so a post-hoc `kubectl logs` returns a window with the evidence already
# gone. Their first gate run came back INCONCLUSIVE and was RIGHT to.
start_mds_capture() { # -> prints the capture path
  local f="$HERE/mds-capture-$(date -u +%Y%m%d-%H%M%S)-$$.log"
  k -n flint-system logs -f --tail=0 deploy/flint-pnfs-mds > "$f" 2>/dev/null &
  echo $! > "$f.pid"
  sleep "${CAPTURE_SETTLE:-2}"   # let the stream attach before the first rep generates traffic
  echo "$f"
}
stop_mds_capture() { # $1=capture path
  local pid; pid=$(cat "$1.pid" 2>/dev/null)
  [ -n "${pid:-}" ] && kill "$pid" 2>/dev/null
  rm -f "$1.pid"
}

substrate_gate() { # $1=since-RFC3339 [$2=streamed capture] -> PASS|FAIL|INCONCLUSIVE:<why>
  local gate="$HERE/stripe-width-gate.py" log rc own=0
  [ -x "$gate" ] || [ -f "$gate" ] || { echo "INCONCLUSIVE:gate-script-absent"; return; }
  if [ -n "${2:-}" ]; then
    # An EMPTY capture means the streamer never attached — that is blindness,
    # and blindness is INCONCLUSIVE. Falling back to a post-hoc fetch here
    # would silently reintroduce the rotation hole this exists to close.
    [ -s "$2" ] || { echo "INCONCLUSIVE:streamed-capture-empty"; return; }
    log=$2
  else
    log=$(mktemp); own=1
    k -n flint-system logs deploy/flint-pnfs-mds --since-time="$1" > "$log" 2>/dev/null
    [ -s "$log" ] || { rm -f "$log"; echo "INCONCLUSIVE:no-mds-log"; return; }
  fi
  python3 "$gate" "$log" >&2; rc=$?
  [ "$own" = 1 ] && rm -f "$log"
  case $rc in
    0) echo PASS;;
    1) echo FAIL;;
    2) echo "INCONCLUSIVE:gate-could-not-ask-the-question";;
    *) echo "INCONCLUSIVE:gate-exit-$rc";;
  esac
}

# `soci push` needs --plain-http (soci does NOT read the certs.d hosts config
# nerdctl uses), and `soci create` REJECTS the flag — so it cannot be applied
# to the whole chain. Verified on soci v0.11.1, runby.
build_index() { # $1=iid $2=registry-flint $3=registry-s3
  ssm_run "$1" "nerdctl pull --hosts-dir /etc/containerd/certs.d $2/$IMG \
    && soci create $2/$IMG \
    && soci push --plain-http $2/$IMG \
    && nerdctl tag $2/$IMG $3/$IMG \
    && soci push --plain-http $3/$IMG" | tail -3
}

pushed_digest() { # digest recorded by push-image, so G-INTEG has a reference
  [ -f "$HERE/.pushed-digest" ] && cat "$HERE/.pushed-digest" || echo none
}

case "${1:-}" in
setup-cluster)
  cn=$(k get nodes --no-headers | grep -v control-plane | tail -1 | awk '{print $1}')
  k label node "$cn" oci-ab/role=client --overwrite
  k cordon "$cn"
  helm --kubeconfig "$KC" upgrade --install flint flint/flint-csi-driver-chart \
    --version 1.43.0 -f "$HERE/values-oci-ab.yaml" --wait --timeout 10m
  k -n kube-system patch cm cilium-config --type merge -p '{"data":{"enable-wireguard":"false"}}'
  k -n kube-system rollout restart ds/cilium && k -n kube-system rollout status ds/cilium --timeout 5m
  k uncordon "$cn"
  k get pods -A | grep -E "flint|pnfs" >&2
  ;;
setup-registries)
  k create secret generic oci-ab-s3 \
    --from-literal=REGISTRY_STORAGE_S3_BUCKET="${BUCKET:?}" \
    --from-literal=REGISTRY_STORAGE_S3_REGION=us-west-1 \
    --from-literal=REGISTRY_STORAGE_S3_ACCESSKEY="${S3_KEY_ID:?}" \
    --from-literal=REGISTRY_STORAGE_S3_SECRETKEY="${S3_SECRET:?}" \
    --dry-run=client -o yaml | k apply -f -
  k apply -f "$HERE/registries.yaml"
  k rollout status deploy/registry-flint --timeout 5m
  k rollout status deploy/registry-s3 --timeout 5m
  ;;
push-image)
  # Push from the NODE (v1 used host docker + two port-forwards on the same
  # local port, which races). Record the digest both registries report: it is
  # G-INTEG's reference and the G4 identity check in one.
  cn=$(client_node); iid=$(node_iid "$cn") || exit 1
  rf=$(reg_ip registry-flint):5000; rs=$(reg_ip registry-s3):5000
  pout=$(ssm_run "$iid" "
    nerdctl pull --quiet $IMG
    for r in $rf $rs; do nerdctl tag $IMG \$r/$IMG; nerdctl --hosts-dir /etc/containerd/certs.d push \$r/$IMG >/dev/null; done
    for r in $rf $rs; do
      d=\$(curl -sI -H 'Accept: application/vnd.oci.image.index.v1+json,application/vnd.docker.distribution.manifest.list.v2+json' http://\$r/v2/${IMG%%:*}/manifests/${IMG##*:} | tr -d '\r' | awk -F': ' '/[Dd]ocker-[Cc]ontent-[Dd]igest/{print \$2}')
      echo \"#RIG registry=\$r digest=\$d\"
    done") || { warn "FATAL: push failed"; exit 1; }
  echo "$pout" >&2
  # G4: both registries must serve the SAME manifest digest.
  #
  # The first version of this check extracted well-formed digests and
  # required the unique set to have size 1 — which PASSES when a registry
  # answers nothing at all, because an empty answer never becomes a set
  # member. flint-29 hit exactly that on runby: registry-s3 was 500ing every
  # blob PUT and 404ing the manifest, one digest came back empty, and G4
  # reported satisfied. It proved "the digests I could parse agree", not
  # "both registries agree" — indistinguishable from absence, which is this
  # campaign's signature failure shape appearing inside its own guard.
  #
  # So: assert the EXPECTED NUMBER OF ANSWERS first, and require a
  # well-formed digest from each named registry, before comparing them.
  # Count what should be there; never just deduplicate what arrived.
  answers=$(echo "$pout" | grep -c "#RIG registry=")
  [ "$answers" -eq 2 ] || { warn "FATAL(G4): expected 2 registry answers, got $answers"; exit 1; }
  missing=$(echo "$pout" | awk '/#RIG registry=/ {
      r=""; d="";
      for (i=1;i<=NF;i++) {
        if ($i ~ /^registry=/) r=substr($i,10);
        if ($i ~ /^digest=/)   d=substr($i,8);
      }
      if (d !~ /^sha256:[0-9a-f]+$/) print r }')
  [ -z "$missing" ] || { warn "FATAL(G4): no usable manifest digest from: $missing"; warn "  (a registry that answers nothing is not a registry that agrees)"; exit 1; }
  digs=$(echo "$pout" | sed -n 's/.*digest=\(sha256:[0-9a-f]*\).*/\1/p' | sort -u)
  n=$(echo "$digs" | grep -c . )
  [ "$n" -eq 1 ] || { warn "FATAL(G4): registries disagree on the manifest digest:"; warn "$digs"; exit 1; }
  echo "$digs" > "$HERE/.pushed-digest"; warn "pushed digest: $digs"
  ;;
setup-client)
  cn=$(client_node); iid=$(node_iid "$cn") || exit 1
  rf=$(reg_ip registry-flint):5000; rs=$(reg_ip registry-s3):5000
  b64=$(base64 < "$HERE/node-soci-setup.sh" | tr -d '\n')
  ssm_run "$iid" "echo $b64 | base64 -d > /tmp/node-soci-setup.sh && REG_FLINT=$rf REG_S3=$rs bash /tmp/node-soci-setup.sh" | tail -6
  build_index "$iid" "$rf" "$rs"
  ;;
install-node)
  # Split out of setup-client: `push-image` needs nerdctl, and nerdctl is
  # what node-soci-setup.sh installs, so the old documented order
  # (push-image then setup-client) could not run on a fresh node.
  cn=$(client_node); iid=$(node_iid "$cn") || exit 1
  rf=$(reg_ip registry-flint):5000; rs=$(reg_ip registry-s3):5000
  b64=$(base64 < "$HERE/node-soci-setup.sh" | tr -d '\n')
  ssm_run "$iid" "echo $b64 | base64 -d > /tmp/node-soci-setup.sh && REG_FLINT=$rf REG_S3=$rs bash /tmp/node-soci-setup.sh" | tail -6
  ;;
build-index)
  cn=$(client_node); iid=$(node_iid "$cn") || exit 1
  build_index "$iid" "$(reg_ip registry-flint):5000" "$(reg_ip registry-s3):5000"
  ;;
stage-blob)
  cn=$(client_node); iid=$(node_iid "$cn") || exit 1
  rf=$(reg_ip registry-flint):5000
  ssm_run "$iid" "nerdctl create --name ab-export $rf/$IMG >/dev/null 2>&1; nerdctl export ab-export | gzip > /tmp/rootfs.tar.gz; nerdctl rm ab-export; ls -la /tmp/rootfs.tar.gz"
  warn "TODO(v2): A4 blob job — alpine + erofs-utils, RWX PVC, mkfs.erofs from /tmp/rootfs.tar.gz"
  ;;
preflight)
  # Everything that must hold BEFORE spending cluster time on reps. Cheap,
  # and it names its own failure.
  rc=0
  r=$(guard_clock) || { warn "$r"; rc=1; }
  cn=$(client_node); [ -n "$cn" ] || { warn "PREFLIGHT: no node labelled oci-ab/role=client"; rc=1; }
  iid=$(node_iid "$cn") || rc=1
  r=$(guard_settle) || { warn "PREFLIGHT: $r"; rc=1; }
  d=$(pushed_digest); [ "$d" != none ] || warn "PREFLIGHT: no .pushed-digest — G-INTEG will be INACTIVE this run"
  # Cheap to fix now, expensive to discover after a paid run: if the MDS is at
  # INFO the stripe-width gate is structurally blind and no run can be
  # certified. Say so while the cluster is still empty.
  if ! k -n flint-system logs deploy/flint-pnfs-mds --since=10m 2>/dev/null | grep -q "Number of DSes in stripe"; then
    warn "PREFLIGHT (advisory): no debug-level layout lines in the last 10m. This cannot"
    warn "  distinguish 'MDS not at debug' from 'no layouts granted recently', so on an idle"
    warn "  cluster it is expected noise. If RUST_LOG=debug is not set, fix it now — the"
    warn "  gate is blind without it ('DEBUG BUILD' in the banner is the BINARY, a different"
    warn "  thing). The run's streamed capture, not this check, is what feeds the gate."
  fi
  [ $rc -eq 0 ] && warn "preflight OK: node=$cn iid=$iid digest=$d"
  exit $rc
  ;;
run)
  cn=$(client_node); iid=$(node_iid "$cn") || exit 1
  rf=$(reg_ip registry-flint):5000; rs=$(reg_ip registry-s3):5000
  dig=$(pushed_digest)
  # $$ in the name, and refuse to reuse: two `run`s inside the same second
  # collided on the timestamp and the second SILENTLY APPENDED to the first,
  # merging two runs' reps into one file with nothing to tell them apart.
  # Found by rig-selftest.sh, which ran the same second twice.
  out="$HERE/results-$(date -u +%Y%m%d-%H%M%S)-$$.ndjson"
  [ -e "$out" ] && { warn "FATAL: $out already exists — refusing to append two runs into one file"; exit 1; }
  # NDJSON, append-only: a rep that dies mid-run leaves every earlier rep
  # readable. v1's hand-assembled array left a truncated file that parsed as
  # nothing at all.
  if ! r=$(guard_clock); then warn "$r"; exit 1; fi
  if ! r=$(guard_settle); then warn "REFUSING TO MEASURE: $r"; exit 1; fi
  run_start=$(now_rfc3339)
  capture=$(start_mds_capture)
  warn "streaming MDS log to $capture for the duration of the run"
  for rep in $(seq 1 "$REPS"); do
    arm_order "$rep" | while read -r armn snap which; do
      case $which in FLINT) reg=$rf;; S3) reg=$rs;; esac
      measure_arm "$armn" "$snap" "$reg" "$iid" "$rep" "$dig" "$which" >> "$out"
    done
    warn "rep $rep/$REPS done"
  done
  # The substrate verdict is a ROW in the results, not a side note: whoever
  # reads this file later must not have to remember to go and check.
  stop_mds_capture "$capture"
  verdict=$(substrate_gate "$run_start" "$capture")
  printf '{"record":"substrate","verdict":"%s","since":"%s","capture":"%s"}\n' \
    "$verdict" "$run_start" "$capture" >> "$out"
  case "$verdict" in
    PASS) warn "substrate gate: PASS — bounded grants present and correctly rotated";;
    FAIL) warn "⛔ substrate gate: FAIL — the fleet is serving wrong stripe maps; these numbers are VOID";;
    *)    warn "⚠ substrate gate: $verdict — this run is NOT CERTIFIED (a gate that could not ask its question is not a pass)";;
  esac
  echo "$out"
  ;;
warm-leg)
  # Falsifiability #1: a warm rep must collapse and must show ~zero backend
  # requests. If a warm run still reports cold-shaped time and traffic, the
  # coldness and attribution instruments are not measuring what we think and
  # NO number from this rig means anything.
  cn=$(client_node); iid=$(node_iid "$cn") || exit 1
  rf=$(reg_ip registry-flint):5000
  measure_arm A1 overlayfs "$rf" "$iid" 0 "$(pushed_digest)" FLINT
  since=$(now_rfc3339)
  ssm_run "$iid" "t0=\$(date +%s%N); nerdctl --snapshotter overlayfs run --rm $rf/$IMG python3 -c '$PYEXEC' >/dev/null 2>&1; echo \"#RIG warm_ms=\$(( (\$(date +%s%N)-t0)/1000000 ))\""
  warn "warm backend requests: $(reg_reqs_since registry-flint "$since") (expect ~0)"
  ;;
broken-lazy-leg)
  # Falsifiability #2: point soci at an image with no index. Lazy must fall
  # back LOUDLY. If this leg reports a normal lazy time, the lazy arm was
  # never lazy and A3/A5 measure nothing.
  cn=$(client_node); iid=$(node_iid "$cn") || exit 1
  rf=$(reg_ip registry-flint):5000
  ssm_run "$iid" "
    nerdctl rmi -f $rf/no-index:1 2>/dev/null
    nerdctl tag $rf/$IMG $rf/no-index:1 && nerdctl --hosts-dir /etc/containerd/certs.d push $rf/no-index:1 >/dev/null
    nerdctl --snapshotter soci rmi -f $rf/no-index:1 2>/dev/null
    t0=\$(date +%s%N)
    nerdctl --snapshotter soci pull --quiet --hosts-dir /etc/containerd/certs.d $rf/no-index:1 >/dev/null 2>&1; rc=\$?
    echo \"#RIG brokenlazy_ms=\$(( (\$(date +%s%N)-t0)/1000000 )) rc=\$rc\"
    journalctl -u soci-snapshotter --since '-2 min' | grep -ci 'no index\|not found' || true"
  ;;
score)
  # Paired per-rep ratios only (§9.4). Refuses to report on too few valid
  # reps rather than quoting a mean over one survivor.
  f=${2:?results.ndjson}
  python3 - "$f" "$MIN_VALID_REPS" <<'PY'
import json,sys,statistics as st
all_rows=[json.loads(l) for l in open(sys.argv[1]) if l.strip()]
rows=[r for r in all_rows if r.get("record")!="substrate"]
sub=[r for r in all_rows if r.get("record")=="substrate"]
verdict=sub[-1]["verdict"] if sub else "MISSING"
minreps=int(sys.argv[2])
voids=[r for r in rows if not r.get("valid")]
ok=[r for r in rows if r.get("valid")]
by={}
for r in ok: by.setdefault(r["rep"],{})[r["arm"]]=r
print(f"rows={len(rows)} valid={len(ok)} void={len(voids)}")
for v in voids: print(f"  VOID rep{v['rep']} {v['arm']}: {v['void']}")
print(f"substrate gate: {verdict}")
# INCONCLUSIVE and MISSING are NOT pass. A ratio measured over a substrate
# whose correctness was never established is the ambiguity this rig exists to
# remove, so the headline is withheld while the per-rep detail still prints.
certified = (verdict == "PASS")
if not certified:
    print(f"HEADLINE WITHHELD — substrate gate is {verdict}, not PASS.")
    print("  A gate that could not ask its question does not certify a run;")
    print("  per-rep numbers below are diagnostics only, not a result.")
pairs=[("A5","A3","flint-vs-s3 backend (the clean attribution)"),
       ("A1","A5","full pull vs lazy, same backend")]
for a,b,label in pairs:
    rs=[(rep,d[a]["ready_ms"]/d[b]["ready_ms"]) for rep,d in sorted(by.items())
        if a in d and b in d and d[b]["ready_ms"]>0]
    if len(rs)<minreps:
        print(f"{a}/{b}: REFUSED — {len(rs)} paired reps < {minreps} required ({label})")
        continue
    v=[x for _,x in rs]
    head = f"median {st.median(v):.3f}  range {min(v):.3f}-{max(v):.3f}  n={len(v)}"
    print(f"{a}/{b}: {head}  ({label})" if certified
          else f"{a}/{b}: [uncertified] {head}  ({label})")
    print("        per-rep: " + ", ".join(f"r{rep}={x:.3f}" for rep,x in rs))
PY
  ;;
substrate-gate)
  # Standalone: verdict over the last N minutes (default 30).
  since=$(date -u -v-"${2:-30}"M +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || date -u -d "-${2:-30} min" +%Y-%m-%dT%H:%M:%SZ)
  v=$(substrate_gate "$since"); warn "substrate gate: $v"
  case "$v" in PASS) exit 0;; FAIL) exit 1;; *) exit 2;; esac
  ;;
teardown-check)
  aws ec2 describe-instances --filters Name=instance-state-name,Values=running,pending,stopping,stopped \
    --query 'length(Reservations[].Instances[])' --output text
  ;;
*) echo "usage: $0 setup-cluster|setup-registries|push-image|setup-client|install-node|build-index|stage-blob|preflight|run|warm-leg|broken-lazy-leg|substrate-gate [min]|score <file>|teardown-check" >&2; exit 2 ;;
esac
