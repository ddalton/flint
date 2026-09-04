#!/usr/bin/env bash
# The flint-lean CLUSTER drill — the Phase 6 legs kind cannot run.
#
#   LEG B  burst wave, N=1000 real pods on real nodes
#   LEG A1 graceful node loss (drain) — the drain barrier must be LOSSLESS
#   LEG A2 abrupt node loss (terminate) — loss must equal the RPO, the
#          bucket must stay coherent, and the rescheduled pod must take
#          the lease over from a node that is GONE
#
# Leg A's trigger is synthetic: the rolesanywhere role has no FIS, so a
# genuine 2-minute spot interruption NOTICE cannot be injected. Drain
# reproduces the graceful outcome (SIGTERM -> preStop -> final barrier)
# and terminate reproduces the abrupt one; the notice path itself stays
# unproven and is recorded as such.
#
# Prereqs: KUBECONFIG pointing at the drill cluster, the flint-lean-drill
# image sideloaded on every node (see sideload.yaml), AWS_PROFILE for the
# terminate leg.
set -u
cd "$(dirname "$0")"

NS=lean-drill
K="kubectl -n $NS"
KC="kubectl"
BUCKET=agentws
BURST_N=${BURST_N:-1000}
PASS=0
FAILED=0
NOTES=()

note() { NOTES+=("$1"); echo "  note: $1"; }
ok()   { echo "  ok: $1"; }
bad()  { echo "  BAD: $1"; }
has()  { [ "$(printf '%s' "$2" | grep -c -- "$1")" -gt 0 ]; }

leg() {
  local name=$1; shift
  echo; echo "── $name"
  if "$@"; then PASS=$((PASS+1)); echo "  PASS"; else FAILED=$((FAILED+1)); echo "  FAIL"; fi
}

mcx()     { $K exec mc -- "$@" 2>/dev/null; }
objcat()  { mcx mc cat "m/$BUCKET/$1"; }
# Resolve the manifest THROUGH the pointer: `.flint/lean/current` is the
# only mutable metadata object and it NAMES the write-once generation
# holding the entries. The pre-pointer `.flint/lean/manifest` key is
# tried LAST — after migration it holds a refusal doc with no
# `.entries`. Reading it first is not merely stale, it is VACUOUS: a
# missing object answers 0/false, so assertions pass by reading nothing.
mbody() {
    local c k
    c=$(objcat "$1/.flint/lean/current")
    if [ -n "$c" ]; then
        k=$(printf '%s' "$c" | jq -r '.entries_key // empty')
        [ -z "$k" ] && return 1
        objcat "$k"
        return
    fi
    objcat "$1/.flint/lean/manifest"
}
objexists(){ mcx mc stat "m/$BUCKET/$1" > /dev/null 2>&1; }
allkeys() { mcx mc ls --recursive --json "m/$BUCKET/$1/" | jq -r --arg p "$1/" 'select(.key)|$p + .key'; }

# ─────────────────────────────────────────────────────────────────────
# LEG B — the burst.
# ─────────────────────────────────────────────────────────────────────
legB_burst() {
  $K delete job burst --ignore-not-found --wait=true --timeout=300s > /dev/null 2>&1
  mcx mc rm --recursive --force "m/$BUCKET/burst" > /dev/null 2>&1

  local t0 peak=0
  t0=$(date +%s)
  sed "s/completions: 1000/completions: $BURST_N/; s/parallelism: 1000/parallelism: $BURST_N/" \
    burst-job.yaml | $KC apply -f - > /dev/null || { bad "apply burst job"; return 1; }

  # Sample the fleet while it ramps. The PEAK simultaneous non-Ready pod
  # count is this leg's anti-vacuity guard: if the pods trickled through
  # a few at a time, nothing was burst and the numbers mean nothing.
  local i running succeeded phase_counts
  for i in $(seq 1 200); do
    phase_counts=$($K get pods -l batch.kubernetes.io/job-name=burst \
      --no-headers 2>/dev/null | awk '{print $3}' | sort | uniq -c | tr '\n' ' ')
    running=$($K get pods -l batch.kubernetes.io/job-name=burst --no-headers 2>/dev/null \
      | grep -cE "Running|Init|Pending|ContainerCreating")
    [ "$running" -gt "$peak" ] && peak=$running
    succeeded=$($K get job burst -o jsonpath='{.status.succeeded}' 2>/dev/null)
    succeeded=${succeeded:-0}
    [ "$succeeded" -ge "$BURST_N" ] && break
    sleep 3
  done
  local t1 elapsed
  t1=$(date +%s); elapsed=$((t1 - t0))

  succeeded=$($K get job burst -o jsonpath='{.status.succeeded}' 2>/dev/null); succeeded=${succeeded:-0}
  local failed
  failed=$($K get job burst -o jsonpath='{.status.failed}' 2>/dev/null); failed=${failed:-0}
  echo "  burst: $succeeded/$BURST_N succeeded, $failed failed, peak concurrent $peak, ${elapsed}s"

  # ANTI-VACUITY: a "burst" that never had a fifth of the fleet in flight
  # at once did not burst.
  [ "$peak" -ge $((BURST_N / 5)) ] || { bad "peak concurrency was $peak of $BURST_N — the fleet never burst, leg vacuous"; return 1; }
  ok "the fleet really did burst: peak $peak pods in flight at once"

  [ "$succeeded" -ge "$BURST_N" ] || { bad "$succeeded/$BURST_N pods completed ($failed failed) — the gate or the barrier did not hold at scale"; return 1; }
  ok "all $BURST_N agents passed their own gate assertion and exited 0"

  # The bucket is the oracle: one manifest and one published file per
  # workspace, counted from a SINGLE listing.
  local keys manifests files
  keys=$(allkeys "burst")
  manifests=$(printf '%s\n' "$keys" | grep -c '/\.flint/lean/current$')
  files=$(printf '%s\n' "$keys" | grep -c '/files/agent\.txt$')
  [ "$manifests" -eq "$BURST_N" ] || { bad "$manifests manifests for $BURST_N workspaces"; return 1; }
  [ "$files" -eq "$BURST_N" ] || { bad "$files published agent.txt for $BURST_N workspaces"; return 1; }
  ok "$BURST_N manifests and $BURST_N published files — every workspace committed"

  # ISOLATION: sampled, because 1000 round trips is its own outage. A
  # cross-contaminated workspace shows up as the wrong index in the body.
  local n idx body miss=0
  for n in $(seq 1 20); do
    idx=$(( (n * 47) % BURST_N ))
    body=$(objcat "burst/ws$idx/files/agent.txt")
    [ "$body" = "agent-$idx" ] || { echo "    ws$idx holds '$body'"; miss=$((miss+1)); }
  done
  [ "$miss" -eq 0 ] || { bad "$miss of 20 sampled workspaces hold another workspace's bytes"; return 1; }
  ok "20 sampled workspaces each hold exactly their own bytes — no cross-contamination at $BURST_N"

  note "burst of $BURST_N: peak $peak concurrent, ${elapsed}s to full completion. The store is in-cluster MinIO on a dedicated node, so this is a CLUSTER-side result — scheduling, the checkout gate, per-workspace isolation and 1000 simultaneous drain barriers. It is NOT a proxy-sizing number and must not be quoted as one."
}

# ─────────────────────────────────────────────────────────────────────
# Leg A fixtures: long-lived pods whose flush floor is an HOUR, so the
# ONLY thing that can publish their work is a drain barrier. That is what
# makes A1 and A2 falsifiable rather than a coin flip against the clock.
# ─────────────────────────────────────────────────────────────────────
nodeloss_manifest() { # $1 = index, $2 = node
  cat <<EOF
apiVersion: apps/v1
kind: Deployment
metadata:
  name: nl$1
  namespace: $NS
spec:
  replicas: 1
  selector: { matchLabels: { app: nl$1 } }
  template:
    metadata: { labels: { app: nl$1 } }
    spec:
      terminationGracePeriodSeconds: 90
      affinity:
        nodeAffinity:
          requiredDuringSchedulingIgnoredDuringExecution:
            nodeSelectorTerms:
              - matchExpressions:
                  - key: "lean-drill/store"
                    operator: NotIn
                    values: ["yes"]
      initContainers:
        - name: flint-sync
          image: flint-lean-drill:cluster
          imagePullPolicy: Never
          restartPolicy: Always
          command: ["/usr/local/bin/flint-sync", "run"]
          env:
            - { name: FLINT_SYNC_BUCKET, value: agentws }
            - { name: FLINT_SYNC_PREFIX, value: "nodeloss/ws$1" }
            - { name: FLINT_SYNC_ROOT, value: /work }
            - { name: FLINT_SYNC_ENDPOINT, value: "http://minio.lean-drill.svc:9000" }
            # ONE HOUR. No periodic barrier can fire inside this drill,
            # so a published file proves the DRAIN barrier ran.
            - { name: FLINT_SYNC_FLOOR_SECS, value: "3600" }
            - { name: AWS_ACCESS_KEY_ID, value: drill }
            - { name: AWS_SECRET_ACCESS_KEY, value: drillsecret }
            - { name: AWS_REGION, value: us-east-1 }
          startupProbe:
            exec: { command: ["/bin/sh", "-c", "test -f /work/.flint-sync/checkout-complete"] }
            periodSeconds: 3
            failureThreshold: 100
          volumeMounts: [{ name: work, mountPath: /work }]
      containers:
        - name: agent
          image: flint-lean-drill:cluster
          imagePullPolicy: Never
          command: ["/bin/sh", "-c", "test -f /work/.flint-sync/checkout-complete || exit 1; sleep infinity"]
          volumeMounts: [{ name: work, mountPath: /work }]
      volumes:
        - name: work
          emptyDir: {}
EOF
}

nl_pod()  { $K get pods -l app=nl$1 -o jsonpath='{.items[0].metadata.name}' 2>/dev/null; }
nl_node() { $K get pods -l app=nl$1 -o jsonpath='{.items[0].spec.nodeName}' 2>/dev/null; }

# ─────────────────────────────────────────────────────────────────────
# LEG A1 — graceful node loss. The claim: an evicted agent loses NOTHING,
# because the sidecar's SIGTERM path runs a final drain barrier.
# ─────────────────────────────────────────────────────────────────────
legA1_drain() {
  local i
  for i in 1 2 3 4; do nodeloss_manifest $i | $KC apply -f - > /dev/null; done
  for i in 1 2 3 4; do
    $K rollout status deploy/nl$i --timeout=300s > /dev/null || { bad "nl$i never came up"; return 1; }
  done
  ok "4 workspaces up behind the checkout gate (flush floor 3600s)"

  # Each agent writes work that NO periodic barrier will ever publish.
  for i in 1 2 3 4; do
    $K exec "$(nl_pod $i)" -c agent -- /bin/sh -c "echo drain-work-$i > /work/late.txt" > /dev/null 2>&1
  done
  sleep 5
  # ANTI-VACUITY: it must be unpublished BEFORE the drain, or "published
  # after" proves nothing.
  local pre=0
  for i in 1 2 3 4; do objexists "nodeloss/ws$i/files/late.txt" && pre=$((pre+1)); done
  [ "$pre" -eq 0 ] || { bad "$pre/4 files were already published before the drain — the floor is not holding, leg vacuous"; return 1; }
  ok "0/4 published before the drain (the 3600s floor holds — only a drain can publish)"

  # Drain the node carrying the most of them.
  local target
  target=$($K get pods -l 'app in (nl1,nl2,nl3,nl4)' -o jsonpath='{range .items[*]}{.spec.nodeName}{"\n"}{end}' \
           | sort | uniq -c | sort -rn | head -1 | awk '{print $2}')
  [ -n "$target" ] || { bad "could not find a node to drain"; return 1; }
  local victims
  victims=$($K get pods -l 'app in (nl1,nl2,nl3,nl4)' \
            -o jsonpath="{range .items[?(@.spec.nodeName=='$target')]}{.metadata.labels.app}{' '}{end}")
  echo "  draining $target (carries: $victims)"

  $KC drain "$target" --ignore-daemonsets --delete-emptydir-data --force \
      --grace-period=90 --timeout=300s > /dev/null 2>&1
  ok "node $target drained"

  # The claim.
  local lost=0 found=0 w
  for w in $victims; do
    i=${w#nl}
    if objexists "nodeloss/ws$i/files/late.txt"; then
      local body
      body=$(objcat "nodeloss/ws$i/files/late.txt")
      [ "$body" = "drain-work-$i" ] && found=$((found+1)) || { bad "ws$i published '$body'"; lost=$((lost+1)); }
    else
      lost=$((lost+1))
    fi
  done
  [ "$found" -gt 0 ] || { bad "no drained workspace published anything — leg vacuous"; return 1; }
  [ "$lost" -eq 0 ] || { bad "$lost of $((found+lost)) drained workspaces LOST their unpublished work"; return 1; }
  ok "graceful node loss is LOSSLESS: all $found evicted workspaces published on the drain barrier"
  note "the drain barrier is what makes a graceful spot reclaim free. A real spot NOTICE was not injectable (no FIS on this role), so what is proven is the OUTCOME of the graceful path, not the notice plumbing that would trigger it."
  $KC uncordon "$target" > /dev/null 2>&1
}

# ─────────────────────────────────────────────────────────────────────
# LEG A2 — abrupt node loss. Loss must equal the RPO, the bucket must
# stay coherent, and a pod rescheduled onto a DIFFERENT node (fresh
# emptyDir ⇒ fresh incarnation) must take the lease over from a holder
# that no longer exists.
# ─────────────────────────────────────────────────────────────────────
legA2_terminate() {
  local i=9
  nodeloss_manifest $i | $KC apply -f - > /dev/null
  $K rollout status deploy/nl$i --timeout=300s > /dev/null || { bad "nl$i never came up"; return 1; }
  local pod node
  pod=$(nl_pod $i); node=$(nl_node $i)

  # A published baseline (forced barrier), then unpublished work on top.
  $K exec "$pod" -c flint-sync -- /bin/sh -c "echo baseline > /work/base.txt" > /dev/null 2>&1
  $K exec "$pod" -c flint-sync -- /usr/local/bin/flint-sync barrier > /dev/null 2>&1 \
    || { bad "forced baseline barrier failed"; return 1; }
  objexists "nodeloss/ws$i/files/base.txt" || { bad "baseline never published"; return 1; }
  $K exec "$pod" -c flint-sync -- /bin/sh -c "echo doomed > /work/doomed.txt" > /dev/null 2>&1
  objexists "nodeloss/ws$i/files/doomed.txt" && { bad "the doomed write published itself — leg vacuous"; return 1; }
  ok "baseline published, unpublished write staged on node $node"

  local iid
  iid=$($KC get node "$node" -o jsonpath='{.spec.providerID}' | awk -F/ '{print $NF}')
  [ -n "$iid" ] || { bad "no providerID for $node"; return 1; }
  echo "  terminating $node ($iid)"
  aws ec2 terminate-instances --region "${AWS_REGION:-us-west-1}" --instance-ids "$iid" > /dev/null 2>&1 \
    || { bad "terminate-instances refused (no ec2:TerminateInstances?) — cannot run this leg"; return 1; }
  ok "instance $iid terminated with no notice"

  # The pod must land somewhere else, with a FRESH emptyDir, and take
  # over a lease whose holder is gone (6 quiet polls).
  local j newpod newnode
  for j in $(seq 1 120); do
    newnode=$(nl_node $i); newpod=$(nl_pod $i)
    [ -n "$newnode" ] && [ "$newnode" != "$node" ] && \
      [ "$($K get pod "$newpod" -o jsonpath='{.status.phase}' 2>/dev/null)" = "Running" ] && break
    sleep 5
  done
  [ "$newnode" != "$node" ] && [ -n "$newnode" ] || { bad "pod never rescheduled off the dead node"; return 1; }
  ok "rescheduled onto $newnode and took over the lease from a node that no longer exists"

  # Loss is EXACTLY the RPO: the baseline survived, the staged write did not.
  objexists "nodeloss/ws$i/files/base.txt" || { bad "the PUBLISHED baseline was lost"; return 1; }
  if objexists "nodeloss/ws$i/files/doomed.txt"; then
    bad "an UNPUBLISHED write survived an abrupt kill — that is not the RPO contract"; return 1
  fi
  ok "loss equals the RPO exactly: baseline survived, unpublished write gone"

  # Coherence + the successor's view.
  local cited present missing
  cited=$(mbody "nodeloss/ws$i" | jq -r '.entries[].key' | sort)
  present=$(allkeys "nodeloss/ws$i" | sort)
  missing=$(comm -23 <(printf '%s\n' "$cited") <(printf '%s\n' "$present") | grep -c .)
  [ "$missing" -eq 0 ] || { bad "$missing dangling citations after abrupt node loss"; return 1; }
  local tree
  tree=$($K exec "$newpod" -c agent -- /bin/sh -c "cd /work && ls | sort | tr '\n' ' '" 2>/dev/null)
  [ "$tree" = "base.txt " ] || { bad "successor tree is '$tree', want exactly 'base.txt '"; return 1; }
  ok "zero dangling citations; the successor's checkout reproduces exactly the published set"
}

# ─────────────────────────────────────────────────────────────────────
echo "flint-lean CLUSTER drill — $(kubectl config current-context 2>/dev/null)"
$K get pod mc > /dev/null 2>&1 || { echo "FAIL: oracle pod mc missing"; exit 1; }
mcx mc alias set m http://minio.lean-drill.svc:9000 drill drillsecret > /dev/null \
  || { echo "FAIL: mc alias"; exit 1; }

leg "B   burst wave, N=$BURST_N"                    legB_burst
leg "A1  graceful node loss (drain) — lossless"     legA1_drain
leg "A2  abrupt node loss (terminate) — loss = RPO" legA2_terminate

echo
echo "════════════════════════════════════════════════"
echo "flint-lean cluster drill: $PASS passed, $FAILED failed (of 3)"
if [ ${#NOTES[@]} -gt 0 ]; then
  echo; echo "Measured / recorded:"
  for n in "${NOTES[@]}"; do echo "  · $n"; done
fi
[ "$FAILED" -eq 0 ]
