#!/usr/bin/env bash
# writers-live drill driver — the DEPLOYED legs (plan §3.1). Runs ON THE
# CONTROL PLANE as root, over SSM, with kubectl on admin.conf. The Mac
# (mac.sh) stages `_rig/`, starts the node shippers, calls one phase at a
# time, and judges each leg (oracle.py needs the node traces, which live
# with the shippers).
#
#   drill.sh fetch                 scripts + binaries from s3://$BUCKET/_rig/ into $RIG
#   drill.sh strip [--yes]         flux to 0 and trove's flint CSI stack out (prints first)
#   drill.sh setup                 charts, broker secret, rollouts — every wait fatal
#   drill.sh leg <A1|A2|A3|A4>     one storm: namespace, CR, agents, run, pause, collect
#   drill.sh idle <A1> <secs> <after-secs>
#                                  A5 on A1's paused fleet: idle, one edit, idle
#   drill.sh collect <leg>         (re)collect a leg into $COLLECT/<leg> and upload it
#   drill.sh freeze <leg>          pause every agent, snapshot state (E7), mark FAILED
#   drill.sh teardown-leg <leg>    stop agents, delete the namespace (never on a frozen leg)
#
# Every wait is fatal (runcu's rig printed SETUP DONE on a failed step).
# A leg never deletes a pod it has not snapshotted first (E7), and a
# frozen leg (`_rig/FAILED-<leg>`) refuses every later phase until the
# marker is removed by hand.
set -uo pipefail

: "${BUCKET:?set BUCKET}"
REGION=${REGION:-us-west-1}
RIG=${RIG:-/mnt/nvme/rig}
COLLECT=${COLLECT:-/mnt/nvme/collect}
EVID=${EVID:-/mnt/nvme/evidence}
AGENTS=${AGENTS:-6}
FLOOR=${FLOOR:-5}
STORM_SECS=${STORM_SECS:-300}
KILL_EVERY=${KILL_EVERY:-30}
AGENT_IMAGE=${AGENT_IMAGE:-busybox:1.36}
export KUBECONFIG=${KUBECONFIG:-/etc/kubernetes/admin.conf}
export AWS_REGION=$REGION AWS_DEFAULT_REGION=$REGION AWS_MAX_ATTEMPTS=5
FSYNC=$RIG/bin/flint-sync
GW=$RIG/bin/flint-lean-gateway

log()    { echo "$(date -u +%H:%M:%S) $*" >&2; }
die()    { echo "$(date -u +%H:%M:%S) FATAL: $*" >&2; exit 1; }
now_ms() { echo $(( $(date +%s%N) / 1000000 )); }
K()      { kubectl "$@"; }

# Per-leg shape. mode, UI actor, faults, the oracles that judge it.
leg_mode()    { case $1 in A1) echo disjoint;; A2) echo hot;; A3) echo churn;; A4) echo vocab;; *) die "no leg $1";; esac; }
leg_ui()      { case $1 in A2|A3) echo 1;; *) echo 0;; esac; }
leg_faults()  { case $1 in A4) echo 1;; *) echo 0;; esac; }
leg_ns()      { echo "wl-$(echo "$1" | tr 'A-Z' 'a-z')"; }

frozen_guard() {
  if aws s3 ls "s3://$BUCKET/_rig/FAILED-$1" >/dev/null 2>&1; then
    die "leg $1 is FROZEN (_rig/FAILED-$1): read the evidence, then remove the marker by hand"
  fi
}

# ── fetch ─────────────────────────────────────────────────────────────
fetch() {
  mkdir -p "$RIG/bin" "$COLLECT" "$EVID" || die "mkdir under /mnt/nvme"
  mountpoint -q /mnt/nvme || log "WARNING: /mnt/nvme is not a mount — the 8 GB root takes the evidence"
  # cp, never sync: sync skips a same-size file, and a rebuilt binary or a
  # re-rendered chart can be exactly the old size (it shipped the old build).
  aws s3 cp --recursive "s3://$BUCKET/_rig/scripts/" "$RIG/" --only-show-errors || die "scripts copy"
  aws s3 cp --recursive "s3://$BUCKET/_rig/bin/" "$RIG/bin/" --only-show-errors || die "binaries copy"
  aws s3 cp "s3://$BUCKET/_rig/TAG" "$RIG/TAG" --only-show-errors || die "tag"
  chmod +x "$RIG"/*.sh "$RIG"/*.py "$RIG"/bin/* 2>/dev/null
  # The binaries' sums were published by the Mac beside them; a truncated
  # copy is a SIGSEGV that reads like a bad cross-compile.
  (cd "$RIG/bin" && aws s3 cp "s3://$BUCKET/_rig/bin.SHA256SUMS" - | sha256sum -c -) || die "binary checksums"
  "$FSYNC" 2>&1 | head -1 >&2
  log "fetched into $RIG"
}

# ── strip: what trove installed that the drill does not want ─────────
strip() {
  K get ds,deploy,sts -A -o wide | grep -i -E 'flint|flux' >&2 || true
  if [ "${1:-}" != --yes ]; then
    log "dry run: re-run 'strip --yes' to scale flux to 0 and delete namespace flint-system"
    return 0
  fi
  if K get ns flux-system >/dev/null 2>&1; then
    K -n flux-system scale deploy --all --replicas=0 >&2 || die "flux scale"
  fi
  if K get ns flint-system >/dev/null 2>&1; then
    K delete ns flint-system --wait=true --timeout=300s >&2 || die "delete flint-system"
  fi
  log "stripped"
}

# ── setup ─────────────────────────────────────────────────────────────
setup() {
  [ -f "$RIG/charts/lean.yaml" ] && [ -f "$RIG/charts/s3csi.yaml" ] || die "rendered charts missing under $RIG/charts"
  # The charts must name the build this rig was staged for — a stale
  # render applies the previous images under a green rollout.
  local tag; tag=$(cat "$RIG/TAG" 2>/dev/null) || die "no $RIG/TAG: run fetch"
  grep -q "flint-lean-operator:$tag" "$RIG/charts/lean.yaml" || die "charts/lean.yaml does not name $tag"
  grep -q "flint-s3-csi:$tag" "$RIG/charts/s3csi.yaml" || die "charts/s3csi.yaml does not name $tag"
  grep -q "flint-s3-worker-lean:$tag" "$RIG/charts/s3csi.yaml" || die "charts/s3csi.yaml's lean worker is not $tag"
  K create ns flint-system --dry-run=client -o yaml | K apply -f - >&2 || die "ns flint-system"
  # Pods cannot reach IMDS behind this CNI (runcu finding 1): the broker
  # and the operator hold the NODE role's credentials as a static Secret.
  local tok role crd akid sak stok
  tok=$(curl -sS -X PUT http://169.254.169.254/latest/api/token -H "X-aws-ec2-metadata-token-ttl-seconds: 300")
  role=$(curl -sS -H "X-aws-ec2-metadata-token: $tok" http://169.254.169.254/latest/meta-data/iam/security-credentials/)
  crd=$(curl -sS -H "X-aws-ec2-metadata-token: $tok" "http://169.254.169.254/latest/meta-data/iam/security-credentials/$role")
  akid=$(python3 -c 'import json,sys;print(json.load(sys.stdin)["AccessKeyId"])' <<<"$crd")
  sak=$(python3 -c 'import json,sys;print(json.load(sys.stdin)["SecretAccessKey"])' <<<"$crd")
  stok=$(python3 -c 'import json,sys;print(json.load(sys.stdin)["Token"])' <<<"$crd")
  [ -n "$akid" ] && [ -n "$sak" ] && [ -n "$stok" ] || die "IMDS credentials empty"
  K -n flint-system create secret generic broker-creds \
    --from-literal=AWS_ACCESS_KEY_ID="$akid" --from-literal=AWS_SECRET_ACCESS_KEY="$sak" \
    --from-literal=AWS_SESSION_TOKEN="$stok" --from-literal=AWS_REGION="$REGION" \
    --dry-run=client -o yaml | K apply -f - >&2 || die "broker secret"
  K apply --server-side --force-conflicts -f "$RIG/charts/s3csi.yaml" >&2 || die "apply s3csi chart"
  K apply --server-side --force-conflicts -f "$RIG/charts/lean.yaml" >&2 || die "apply lean chart"
  K -n flint-system rollout status deploy/flint-s3-broker --timeout=300s >&2 || die "broker rollout"
  K -n flint-system rollout status ds/flint-s3-csi-node --timeout=300s >&2 || die "csi-node rollout"
  K -n flint-system rollout status deploy/flint-lean --timeout=300s >&2 || die "operator rollout"
  # The images the pods RUN, by digest — a tag is mutable.
  K -n flint-system get pods -o jsonpath='{range .items[*]}{.metadata.name}{"\t"}{range .status.containerStatuses[*]}{.image}{" "}{.imageID}{"\n"}{end}{end}' >&2
  log "setup done"
}

# ── manifests ─────────────────────────────────────────────────────────
render_leg() { # <leg> <ns> <prefix>
  local leg=$1 ns=$2 prefix=$3 mode
  mode=$(leg_mode "$leg")
  cat <<EOF
apiVersion: v1
kind: Namespace
metadata: { name: $ns, labels: { writers-live: "$leg" } }
---
apiVersion: v1
kind: ServiceAccount
metadata: { name: agent, namespace: $ns }
---
apiVersion: chert.us/v1alpha1
kind: FlintLeanWorkspace
metadata: { name: ws, namespace: $ns }
spec:
  projectId: writers-$(echo "$leg" | tr 'A-Z' 'a-z')
  bucket: $BUCKET
  keyPrefix: $prefix
  region: $REGION
  uid: 1001
  gid: 1001
  floorSecs: $FLOOR
  fetchInflightMb: 128
  sizeLimitGib: 1
  eventTrace: true
  consumers: { serviceAccounts: [agent] }
  identity: { mode: broker }
---
apiVersion: apps/v1
kind: StatefulSet
metadata: { name: agents, namespace: $ns }
spec:
  serviceName: agents
  replicas: $AGENTS
  podManagementPolicy: Parallel
  selector: { matchLabels: { app: agents } }
  template:
    metadata: { labels: { app: agents } }
    spec:
      serviceAccountName: agent
      terminationGracePeriodSeconds: 60
      securityContext: { runAsUser: 1001, runAsGroup: 1001, runAsNonRoot: true, fsGroup: 1001 }
      topologySpreadConstraints:
        # Honor: the tainted control plane is otherwise a domain holding
        # zero agents, so maxSkew 1 lets no worker take a second one —
        # six agents on three workers left three Pending (two fit).
        - maxSkew: 1
          topologyKey: kubernetes.io/hostname
          whenUnsatisfiable: DoNotSchedule
          nodeTaintsPolicy: Honor
          labelSelector: { matchLabels: { app: agents } }
      initContainers:
        # Every agent starts PAUSED: the driver lifts the pause on all six
        # at once, so no agent storms alone while its peers still mount.
        - name: paused
          image: $AGENT_IMAGE
          command: [sh, -c, "touch /agent/pause"]
          volumeMounts: [ { name: agent, mountPath: /agent } ]
      containers:
        - name: agent
          image: $AGENT_IMAGE
          # \$\$ renders \$\$ and Kubernetes turns it into \$: a bare \$(( would
          # be read as a \$(VAR) reference.
          command:
            - sh
            - -c
            - 'export AGENT_ID=\$HOSTNAME AGENT_SEED=\$\$(( \${HOSTNAME##*-} + 1000 )); exec sh /rig/agent.sh'
          env:
            - { name: AGENT_MODE, value: "$mode" }
            - { name: TREE, value: /work }
            - { name: JOURNAL, value: /agent/journal.jsonl }
            - { name: CONTROL_DIR, value: /agent }
            - { name: FLOOR_SECS, value: "$FLOOR" }
          volumeMounts:
            - { name: work, mountPath: /work }
            - { name: agent, mountPath: /agent }
            - { name: rig, mountPath: /rig }
      volumes:
        - name: work
          csi: { driver: s3.csi.chert.us, volumeAttributes: { chert.us/workspace: ws } }
        - name: agent
          emptyDir: {}
        - name: rig
          configMap: { name: rig }
EOF
}

agent_pods() { K -n "$1" get pods -l app=agents -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}' | sort; }
pod_node()   { K -n "$1" get pod "$2" -o jsonpath='{.spec.nodeName}'; }
inpod()      { local ns=$1 pod=$2; shift 2; K -n "$ns" exec "$pod" -c agent -- sh -c "$*"; }

wait_ready() { # <ns> <n> <timeout>
  local ns=$1 n=$2 t=$3 i ready
  for i in $(seq 1 "$t"); do
    ready=$(K -n "$ns" get pods -l app=agents -o jsonpath='{range .items[*]}{.status.containerStatuses[0].ready}{"\n"}{end}' 2>/dev/null | grep -c true)
    [ "$ready" -ge "$n" ] && return 0
    sleep 1
  done
  K -n "$ns" get pods -o wide >&2
  K -n "$ns" get events --sort-by=.lastTimestamp | tail -20 >&2
  return 1
}

# Wait until every agent's journal carries a line of kind $2 AFTER the
# control file was placed (t_ms >= $3).
wait_journal_kind() { # <ns> <kind> <since_ms> <timeout_s>
  local ns=$1 kind=$2 since=$3 t=$4 i pod missing
  for i in $(seq 1 "$t"); do
    missing=0
    for pod in $(agent_pods "$ns"); do
      if ! inpod "$ns" "$pod" "grep '\"k\":\"$kind\"' /agent/journal.jsonl 2>/dev/null | tail -1" \
           | python3 -c "import json,sys; l=sys.stdin.read().strip(); sys.exit(0 if l and json.loads(l).get('t_ms',0) >= $since else 1)" 2>/dev/null; then
        missing=$((missing + 1))
      fi
    done
    [ "$missing" -eq 0 ] && return 0
    sleep 2
  done
  log "$missing agent(s) never journaled '$kind' since $since"
  return 1
}

# Quiesce: every floor, the pointer seq; settled when unchanged for 4 floors.
wait_quiesce() { # <prefix> <timeout_s>
  local prefix=$1 t=$2 start last same=0 cur
  start=$(date +%s); last=""
  while [ $(( $(date +%s) - start )) -lt "$t" ]; do
    cur=$(manifest_json "$prefix" | python3 -c 'import json,sys; print(json.load(sys.stdin)["seq"])')
    if [ -n "$cur" ] && [ "$cur" = "$last" ]; then same=$((same + 1)); else same=0; fi
    last=$cur
    [ "$same" -ge 4 ] && { log "quiesced at seq $cur"; return 0; }
    sleep "$FLOOR"
  done
  log "no quiesce within ${t}s (last seq $last)"
  return 1
}

manifest_json() { # <prefix>  (a scratch root: the verb opens no state)
  local scratch
  scratch=$(mktemp -d /mnt/nvme/rig/mf.XXXX)
  FLINT_SYNC_BUCKET=$BUCKET FLINT_SYNC_PREFIX=$1 FLINT_SYNC_ROOT=$scratch "$FSYNC" manifest
  rm -rf "$scratch"
}

# ── gateway + UI actor (A6), on this host: IMDS works here ────────────
ui_start() { # <leg> <prefix> <mode>
  local leg=$1 prefix=$2 mode=$3 dir=$COLLECT/$1/ui
  mkdir -p "$dir" && rm -f "$dir/stop" "$dir/pause"
  head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n' > "$dir/token"
  FLINT_LEAN_GW_LISTEN=127.0.0.1:8091 FLINT_LEAN_GW_BUCKET=$BUCKET \
  FLINT_LEAN_GW_TOKEN="$(cat "$dir/token")" FLINT_LEAN_GW_WORKSPACES="ws=$prefix" \
    setsid nohup "$GW" > "$dir/gateway.log" 2>&1 < /dev/null &
  echo $! > "$dir/gateway.pid"
  local i
  for i in $(seq 1 30); do
    curl -s -o /dev/null -H "Authorization: Bearer $(cat "$dir/token")" http://127.0.0.1:8091/lean/v1/ws/status && break
    sleep 1
  done
  [ "$i" -lt 30 ] || die "gateway never answered: $(tail -5 "$dir/gateway.log")"
  GATEWAY=http://127.0.0.1:8091 WORKSPACE=ws GATEWAY_TOKEN_FILE=$dir/token UI_MODE=$mode UI_SEED=77 \
  JOURNAL=$dir/journal.jsonl CONTROL_DIR=$dir AGENT_SH=$RIG/agent.sh \
    setsid nohup sh "$RIG/ui.sh" > "$dir/ui.log" 2>&1 < /dev/null &
  echo $! > "$dir/ui.pid"
  log "UI actor started ($mode) against the gateway"
}
ui_stop() { # <leg>
  local dir=$COLLECT/$1/ui i
  [ -d "$dir" ] || return 0
  touch "$dir/stop"
  for i in $(seq 1 60); do kill -0 "$(cat "$dir/ui.pid")" 2>/dev/null || break; sleep 1; done
  kill "$(cat "$dir/gateway.pid")" 2>/dev/null
  log "UI actor stopped"
}

# ── A4's faults: a worker pod deleted, a syncer SIGKILLed, alternately ─
workers_of() { # <ns>  → worker pod names serving this namespace's agents
  K -n flint-workers get pods -o json | python3 -c "
import json,sys
ns=sys.argv[1]
for p in json.load(sys.stdin)['items']:
    a=p['metadata'].get('annotations',{})
    if a.get('chert.us/tenant-pod','').startswith(ns+'/') or p['metadata'].get('labels',{}).get('chert.us/tenant-namespace')==ns:
        print(p['metadata']['name'], a.get('chert.us/tenant-pod',''), p['spec'].get('nodeName',''))
" "$1"
}
faults_loop() { # <leg> <ns> <until_epoch>
  # Every fault must TAKE EFFECT, or the leg is VOID: the first A4 ran
  # three, and both worker deletions were refused by the workers
  # namespace's admission policy (only the CSI node's identity, the
  # garbage collectors, or the node's own kubelet may delete a worker)
  # while the loop logged the refusal and carried on — a fault leg that
  # passed with one fault. A fault that does not land writes faults.void
  # and stops injecting; the storm, the pause and the collect go on, and
  # the verdict reports VOID.
  local leg=$1 ns=$2 until=$3 round=0 victim tenant node line gone
  mkdir -p "$COLLECT/$leg/faults"
  while [ "$(date +%s)" -lt "$until" ]; do
    sleep "$KILL_EVERY"
    [ "$(date +%s)" -lt "$until" ] || break
    round=$((round + 1))
    line=$(workers_of "$ns" | shuf -n 1)
    read -r victim tenant node <<< "$line"
    if [ -z "$victim" ] || [ -z "$node" ]; then
      echo "round $round: no worker found" >> "$COLLECT/$leg/faults.void"; log "fault $round: no worker: VOID"; break
    fi
    # E7 BEFORE anything is killed: what the writer believed.
    snapshot_state "$ns" "${tenant#*/}" "$COLLECT/$leg/faults/$round-before" || true
    if [ $((round % 2)) -eq 1 ]; then
      log "fault $round: delete worker $victim on $node (tenant $tenant), as that node's kubelet"
      if ! K --as="system:node:$node" --as-group=system:nodes --as-group=system:authenticated \
          -n flint-workers delete pod "$victim" --wait=false >&2; then
        echo "round $round: delete of $victim refused" >> "$COLLECT/$leg/faults.void"; log "fault $round: refused: VOID"; break
      fi
      gone=$(K -n flint-workers get pod "$victim" -o jsonpath='{.metadata.deletionTimestamp}' 2>/dev/null || echo gone)
      if [ -z "$gone" ]; then
        echo "round $round: $victim has no deletionTimestamp after delete" >> "$COLLECT/$leg/faults.void"; log "fault $round: not deleting: VOID"; break
      fi
      echo "{\"t_ms\":$(now_ms),\"round\":$round,\"fault\":\"delete-worker\",\"worker\":\"$victim\",\"node\":\"$node\",\"tenant\":\"$tenant\"}" >> "$COLLECT/$leg/faults.jsonl"
    else
      # Aim at the commit section: wait (bounded) for the victim's trace
      # to show it has just CLAIMED the cell, then kill at once. Whether
      # it landed inside is judged afterwards from the traces (a claim
      # with no release before the restart), never assumed.
      log "fault $round: SIGKILL flint-sync in $victim (tenant $tenant) at its next claim"
      timeout 45 kubectl -n flint-workers logs -f --since=1s "$victim" 2>/dev/null \
        | grep -m1 -E '"ev":"claim".*"verdict":"claimed"' >/dev/null || log "fault $round: no claim seen in 45 s; killing anyway"
      if ! K -n flint-workers exec "$victim" -- sh -c 'pkill -9 -x flint-sync || kill -9 $(pidof flint-sync)' >&2; then
        echo "round $round: SIGKILL in $victim did not land" >> "$COLLECT/$leg/faults.void"; log "fault $round: kill failed: VOID"; break
      fi
      echo "{\"t_ms\":$(now_ms),\"round\":$round,\"fault\":\"kill-9-syncer\",\"worker\":\"$victim\",\"tenant\":\"$tenant\"}" >> "$COLLECT/$leg/faults.jsonl"
    fi
  done
}

snapshot_state() { # <ns> <pod> <outdir>
  local ns=$1 pod=$2 out=$3
  mkdir -p "$out"
  pull_tar "$ns" "$pod" "cd /work && tar czf /tmp/state.tgz .flint-sync .flint 2>/dev/null; echo done" /tmp/state.tgz "$out/state.tar.gz"
}

# A tarball out of a pod with its sha256 checked at both ends (kubectl cp
# truncates silently and exits 0).
pull_tar() { # <ns> <pod> <make-cmd> <remote> <local>
  local ns=$1 pod=$2 mk=$3 remote=$4 dst=$5 want got try
  for try in 1 2 3; do
    inpod "$ns" "$pod" "$mk" >/dev/null 2>&1
    want=$(inpod "$ns" "$pod" "sha256sum $remote" | awk '{print $1}')
    K -n "$ns" exec "$pod" -c agent -- cat "$remote" > "$dst" 2>/dev/null
    got=$(sha256sum "$dst" | awk '{print $1}')
    [ -n "$want" ] && [ "$want" = "$got" ] && return 0
    log "pull $ns/$pod:$remote try $try: sha mismatch ($want vs $got)"
  done
  return 1
}

# ── collect ───────────────────────────────────────────────────────────
collect() { # <leg>
  local leg=$1 ns prefix dir pod node agents_json nodes_json code
  ns=$(leg_ns "$leg"); dir=$COLLECT/$leg
  prefix=$(cat "$dir/prefix") || die "no prefix recorded for $leg"
  mkdir -p "$dir/agents" "$dir/bucket" "$dir/checkout"
  agents_json="["; nodes_json="{"
  for pod in $(agent_pods "$ns"); do
    node=$(pod_node "$ns" "$pod")
    mkdir -p "$dir/agents/$pod"
    # Tree digest in the pod: every regular file, paths relative to the
    # tree, excluding the syncer's namespaces and in-flight temp names.
    pull_tar "$ns" "$pod" "cd /work && find . -type f | sed 's|^\./||' \
        | grep -v -E '(^|/)\.flint(-sync)?(/|$)' | grep -v -E '\.flint-sync-tmp$' | sort \
        | while IFS= read -r f; do sha256sum \"\$f\"; done > /tmp/tree.sha256; \
        mkdir -p /tmp/c && cp /agent/journal.jsonl /tmp/tree.sha256 /tmp/c/ && \
        cp /work/.flint-sync/conflicts*.jsonl /tmp/c/ 2>/dev/null; \
        tar czf /tmp/state.tgz -C /work .flint-sync .flint 2>/dev/null; cp /tmp/state.tgz /tmp/c/; \
        tar czf /tmp/collect.tgz -C /tmp/c . ; echo ok" /tmp/collect.tgz "$dir/agents/$pod/collect.tgz" \
      || die "collect $pod: checksum never matched"
    tar xzf "$dir/agents/$pod/collect.tgz" -C "$dir/agents/$pod" || die "untar $pod"
    mv "$dir/agents/$pod/state.tgz" "$dir/agents/$pod/state.tar.gz" 2>/dev/null
    [ -f "$dir/agents/$pod/conflicts.jsonl" ] || : > "$dir/agents/$pod/conflicts.jsonl"
    agents_json="$agents_json\"$pod\","; nodes_json="$nodes_json\"$pod\":\"$node\","
  done
  [ -f "$dir/ui/journal.jsonl" ] && { mkdir -p "$dir/agents/ui"; cp "$dir/ui/journal.jsonl" "$dir/agents/ui/"; nodes_json="$nodes_json\"ui\":\"$(hostname)\","; }
  agents_json="${agents_json%,}]"; nodes_json="${nodes_json%,}}"

  # The bucket, as the store reports it.
  manifest_json "$prefix" > "$dir/bucket/manifest-full.json" || die "manifest verb"
  python3 - "$dir/bucket" <<'PY' || die "split manifest"
import json, sys, os
d = sys.argv[1]
m = json.load(open(os.path.join(d, "manifest-full.json")))
json.dump({"seq": m["seq"], "entries": m["entries"]}, open(os.path.join(d, "manifest.json"), "w"))
json.dump({p: {"etag": e} for p, e in m["heads"].items()}, open(os.path.join(d, "heads.json"), "w"))
PY
  aws s3api list-objects-v2 --bucket "$BUCKET" --prefix "$prefix/" --output json \
    | python3 -c 'import json,sys; d=json.load(sys.stdin); print(json.dumps([{"key":o["Key"],"etag":o["ETag"],"size":o["Size"],"last_modified":o["LastModified"]} for o in d.get("Contents",[])]))' \
    > "$dir/bucket/listing.json" || die "listing"
  python3 - "$dir/bucket/listing.json" "$prefix" "$BUCKET" > "$dir/bucket/preserved.json" <<'PY' || die "preserved"
import json, sys, subprocess, hashlib
listing, prefix, bucket = json.load(open(sys.argv[1])), sys.argv[2], sys.argv[3]
out = []
for o in listing:
    if o["key"].startswith(prefix + "/.flint/lean/conflicts/"):
        body = subprocess.run(["aws", "s3", "cp", f"s3://{bucket}/{o['key']}", "-"], capture_output=True, check=True).stdout
        out.append({"key": o["key"], "etag": o["etag"], "sha256": hashlib.sha256(body).hexdigest()})
print(json.dumps(out))
PY

  # A fresh reader checkout, as a new agent would get it.
  rm -rf "$dir/checkout/tree" && mkdir -p "$dir/checkout/tree"
  FLINT_SYNC_BUCKET=$BUCKET FLINT_SYNC_PREFIX=$prefix FLINT_SYNC_ROOT=$dir/checkout/tree \
    "$FSYNC" checkout > "$dir/checkout/stdout.log" 2> "$dir/checkout/stderr.log"
  code=$?
  python3 -c 'import json,sys; print(json.dumps({"code": int(sys.argv[1]), "stderr_tail": open(sys.argv[2]).read()[-4000:]}))' \
    "$code" "$dir/checkout/stderr.log" > "$dir/checkout/exit.json"
  (cd "$dir/checkout/tree" && find . -type f | sed 's|^\./||' | grep -v -E '(^|/)\.flint(-sync)?(/|$)' \
     | grep -v -E '\.flint-sync-tmp$' | sort | while IFS= read -r f; do sha256sum "$f"; done) > "$dir/checkout/tree.sha256"

  python3 - "$dir" "$leg" "$prefix" "$FLOOR" "$agents_json" "$nodes_json" <<'PY' || die "meta"
import json, sys, os
d, leg, prefix, floor, agents, nodes = sys.argv[1:7]
meta = {"leg": leg, "prefix": prefix, "floor_secs": int(floor), "agents": json.loads(agents), "agent_nodes": json.loads(nodes)}
for k in ("t_start_ms", "t_quiesce_ms", "t_end_ms", "t_idle_from_ms", "t_idle_to_ms"):
    f = os.path.join(d, k)
    if os.path.exists(f):
        meta[k] = int(open(f).read().strip())
json.dump(meta, open(os.path.join(d, "meta.json"), "w"), indent=1)
PY
  (cd "$COLLECT" && tar czf "/mnt/nvme/rig/collect-$leg.tgz" "$leg" && sha256sum "/mnt/nvme/rig/collect-$leg.tgz" | awk '{print $1}' > "/mnt/nvme/rig/collect-$leg.sha256")
  aws s3 cp "/mnt/nvme/rig/collect-$leg.tgz" "s3://$BUCKET/_rig/collect/$leg.tgz" --only-show-errors || die "upload collect"
  aws s3 cp "/mnt/nvme/rig/collect-$leg.sha256" "s3://$BUCKET/_rig/collect/$leg.sha256" --only-show-errors || die "upload sha"
  log "collected $leg: $(cat "/mnt/nvme/rig/collect-$leg.sha256")"
}

# ── a storm leg ───────────────────────────────────────────────────────
leg() { # <leg>
  local leg=$1 ns prefix dir pod since until
  frozen_guard "$leg"
  ns=$(leg_ns "$leg"); dir=$COLLECT/$leg
  [ -e "$dir" ] && die "$dir exists: a leg is never re-run into an old collect dir"
  mkdir -p "$dir"
  prefix="writers/$(echo "$leg" | tr 'A-Z' 'a-z')-$(date -u +%Y%m%d%H%M%S)"
  echo "$prefix" > "$dir/prefix"
  log "leg $leg: ns $ns, prefix $prefix, mode $(leg_mode "$leg"), agents $AGENTS, storm ${STORM_SECS}s"

  K create ns "$ns" --dry-run=client -o yaml | K apply -f - >&2 || die "ns"
  K -n "$ns" create configmap rig --from-file=agent.sh="$RIG/agent.sh" --dry-run=client -o yaml | K apply -f - >&2 || die "configmap"
  render_leg "$leg" "$ns" "$prefix" > "$dir/manifests.yaml"
  K apply -f "$dir/manifests.yaml" >&2 || die "apply leg manifests"
  wait_ready "$ns" "$AGENTS" 900 || die "agents never Ready"
  K -n "$ns" get pods -o wide >&2

  # E6: the bucket history for this prefix, until the leg's end.
  setsid nohup python3 "$RIG/sampler.py" --bucket "$BUCKET" --prefix "$prefix" \
    --out "/mnt/nvme/history/$leg" --interval-ms 500 --until-file "$dir/sampler.stop" \
    > "$dir/sampler.log" 2>&1 < /dev/null &
  echo $! > "$dir/sampler.pid"

  [ "$(leg_ui "$leg")" = 1 ] && ui_start "$leg" "$prefix" "$(leg_mode "$leg")"

  now_ms > "$dir/t_start_ms"
  for pod in $(agent_pods "$ns"); do inpod "$ns" "$pod" "rm -f /agent/pause" || die "unpause $pod"; done
  log "storm started"
  until=$(( $(date +%s) + STORM_SECS ))
  if [ "$(leg_faults "$leg")" = 1 ]; then
    faults_loop "$leg" "$ns" "$until"
  fi
  while [ "$(date +%s)" -lt "$until" ]; do
    sleep 10
    K -n "$ns" get pods -l app=agents --no-headers 2>/dev/null | awk '$3 != "Running" {print "  not running: " $0}' >&2
  done

  # Pause (never stop: a stopped agent exits and its container restarts
  # into a resumed storm), let the last batches take their acks.
  since=$(now_ms)
  for pod in $(agent_pods "$ns"); do inpod "$ns" "$pod" "touch /agent/pause"; done
  [ "$(leg_ui "$leg")" = 1 ] && ui_stop "$leg"
  wait_journal_kind "$ns" paused "$since" $(( FLOOR * 12 + 60 )) || { freeze "$leg"; die "agents never paused"; }
  wait_quiesce "$prefix" 300 || { freeze "$leg"; die "no quiesce"; }
  now_ms > "$dir/t_quiesce_ms"
  # Four floors past the settled seq, so every writer's consume has run.
  sleep $(( FLOOR * 4 ))
  now_ms > "$dir/t_end_ms"
  touch "$dir/sampler.stop"
  collect "$leg"
  log "leg $leg run + collect done; judge it from the Mac (mac.sh verdict $leg)"
}

# ── A5: an idle fleet, one edit, idle again ───────────────────────────
idle() { # <leg-with-paused-fleet> <idle_secs> <after_secs>
  local leg=$1 idle_secs=$2 after=$3 ns dir pod prefix
  frozen_guard "$leg"
  ns=$(leg_ns "$leg"); dir=$COLLECT/A5; prefix=$(cat "$COLLECT/$leg/prefix")
  [ -e "$dir" ] && die "$dir exists"
  mkdir -p "$dir" && echo "$prefix" > "$dir/prefix"
  setsid nohup python3 "$RIG/sampler.py" --bucket "$BUCKET" --prefix "$prefix" \
    --out /mnt/nvme/history/A5 --interval-ms 1000 --until-file "$dir/sampler.stop" \
    > "$dir/sampler.log" 2>&1 < /dev/null &
  now_ms > "$dir/t_idle_from_ms"
  log "A5: $AGENTS paused agents on $prefix, idle ${idle_secs}s"
  sleep "$idle_secs"
  now_ms > "$dir/t_idle_to_ms"
  pod=$(agent_pods "$ns" | head -1)
  local before after_seq
  before=$(manifest_json "$prefix" | python3 -c 'import json,sys; print(json.load(sys.stdin)["seq"])')
  inpod "$ns" "$pod" "echo 'the one edit' > /work/$pod/a5-edit.txt"
  log "A5: one edit on $pod (seq before $before); idle ${after}s"
  sleep "$after"
  after_seq=$(manifest_json "$prefix" | python3 -c 'import json,sys; print(json.load(sys.stdin)["seq"])')
  echo "{\"seq_before_edit\": $before, \"seq_after\": $after_seq}" > "$dir/edit.json"
  log "A5: seq $before -> $after_seq (the control: exactly +1)"
  touch "$dir/sampler.stop"
  now_ms > "$dir/t_end_ms"
  aws s3 cp "$dir/edit.json" "s3://$BUCKET/_rig/collect/A5-edit.json" --only-show-errors
}

# ── freeze ────────────────────────────────────────────────────────────
freeze() { # <leg>
  local leg=$1 ns pod
  ns=$(leg_ns "$leg")
  log "FREEZE $leg: pausing agents, snapshotting every writer, marking FAILED"
  for pod in $(agent_pods "$ns"); do inpod "$ns" "$pod" "touch /agent/pause" 2>/dev/null; done
  ui_stop "$leg" 2>/dev/null
  for pod in $(agent_pods "$ns"); do snapshot_state "$ns" "$pod" "$COLLECT/$leg/frozen/$pod" || true; done
  touch "$COLLECT/$leg/sampler.stop"
  aws s3 sync "$COLLECT/$leg" "s3://$BUCKET/_rig/frozen/$leg/" --only-show-errors
  echo "{\"leg\":\"$leg\",\"t_ms\":$(now_ms)}" | aws s3 cp - "s3://$BUCKET/_rig/FAILED-$leg"
}

teardown_leg() { # <leg>
  local leg=$1 ns
  frozen_guard "$leg"
  ns=$(leg_ns "$leg")
  K delete ns "$ns" --wait=true --timeout=300s >&2 || die "delete $ns"
  log "leg $leg namespace deleted"
}

cmd=${1:-}; shift || true
case "$cmd" in
  fetch) fetch ;;
  strip) strip "$@" ;;
  setup) setup ;;
  leg) leg "${1:?leg}" ;;
  idle) idle "${1:?leg}" "${2:?idle secs}" "${3:?after secs}" ;;
  collect) collect "${1:?leg}" ;;
  freeze) freeze "${1:?leg}" ;;
  teardown-leg) teardown_leg "${1:?leg}" ;;
  *) sed -n '2,20p' "$0"; exit 2 ;;
esac
