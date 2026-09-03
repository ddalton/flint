#!/usr/bin/env bash
# RETIRED PATH (2026-09-03): the lean webhook and sidecar injector are
# gone — a workspace reaches a pod as ONE csi: volume served by the
# s3.csi.chert.us node driver (docs/plans/csi-node-mount-design.md §3.5).
# This rig labels pods and/or execs into an injected `flint-sync`
# container, so it no longer runs as written. The CSI delivery of lean
# is drilled by s3csi/e2e/run-s3csi.sh (S11, S13) and, across clusters,
# s3csi/e2e/multi/run-multi.sh (M3). The PROTOCOL suites here (B1-B25,
# C1-C12) remain the lean ORACLE and are to be re-targeted at the
# worker pod in flint-workers (design §10.2 S12) — not deleted, and
# never left silently green.
# The agent-pod use case, end to end, against the RELEASED operator.
#
# The question this rig answers is the one a fleet operator actually
# asks: what does an agent pod have to do to get its project, and what
# does it get? The answer under test is "one label, and a full local
# checkout that is already there before its first instruction runs" —
# not an S3 endpoint the agent is expected to speak.
#
# House rules inherited from run-chaos.sh / run-verbs.sh: every leg
# observes its own PRECONDITION or FAILS, every refusal has an accepted
# control, and NO LEG MAY PASS BY NOT LOOKING. The two structural
# guards here are (1) floorSecs is an HOUR on both workspaces, so no
# publish a leg sees can be a cadence tick, and (2) the checkout legs
# run in a pod that did not exist when the data was written.
#
# Prereqs: a cluster with the flint-lean chart installed (operator +
# webhook + gateway), MinIO reachable at minio.flint-system.svc:9000,
# and bucket `agentfleet`. `setup` does the MinIO/bucket half.
#
#   kubectl -n flint-system create secret generic gw-token \
#     --from-literal=token=agentfleetgatewaytoken0
#   helm install flint-lean oci://registry-1.docker.io/dilipdalton/flint-lean \
#     --version 0.3.0 -n flint-system --create-namespace \
#     --set operatorCredentialsSecret=minio-creds \
#     --set gateway.enabled=true --set gateway.bucket=agentfleet \
#     --set gateway.endpoint=http://minio.flint-system.svc:9000 \
#     --set gateway.workspaces='proj=tenants/proj,other=tenants/other' \
#     --set gateway.tokenSecret=gw-token --set gateway.credentialsSecret=minio-creds
#
#   CTX=kind-<cluster> ./run-agent.sh setup   # MinIO + bucket + workspaces
#   CTX=kind-<cluster> ./run-agent.sh         # the legs
#
# Proven 2026-08-26 on kind-flint-lean-verbs against chart flint-lean
# 0.3.0 and images flint-lean-operator:1.38.0 / flint-sync:1.38.0 pulled
# from Docker Hub — the released artifacts, not local builds: 9/9 legs,
# 15 assertions. The headline leg is falsifiable, not merely green:
# emptying the seeded project from the bucket makes A2 read 0 files
# where the green run reads 200.
set -u
cd "$(dirname "$0")"

CTX=${CTX:-kind-lean-agent}
K="kubectl --context $CTX"
NS=agents
BUCKET=agentfleet
GWTOKEN=${GWTOKEN:-agentfleetgatewaytoken0}
GW_SVC=${GW_SVC:-flint-lean-gateway.flint-system.svc}
GW_PORT=${GW_PORT:-8091}

PASS=0
FAILED=0
bad()  { echo "  BAD: $1"; FAILED=$((FAILED + 1)); }
ok()   { PASS=$((PASS + 1)); echo "  ok: $1"; }
note() { echo "  NOTE: $1"; }
leg()  { echo; echo "── $1"; }

# ── store helpers ────────────────────────────────────────────────────
mcx() { $K -n flint-system exec mc-agent -- "$@" 2>/dev/null; }
objcat() { mcx mc cat "m/$BUCKET/$1"; }
objcount() { mcx mc ls --recursive "m/$BUCKET/$1" | grep -c . ; }
# jq, not grep: the manifest is nested JSON and a text match for a
# path would also hit it inside a conflict record or a withheld set.
mseq() { local m; m=$(objcat "$1/.flint/lean/manifest"); [ -z "$m" ] && { echo 0; return; }; printf '%s' "$m" | jq -r '.seq // 0'; }
mhas() { local m; m=$(objcat "$1/.flint/lean/manifest"); [ -z "$m" ] && return 1; printf '%s' "$m" | jq -e --arg p "$2" '.entries | has($p)' > /dev/null; }

# ── gateway helpers ──────────────────────────────────────────────────
# A durable curl pod, not a per-call `kubectl run --rm`: an image pull
# inside a leg turns a timing assertion into a registry latency
# measurement.
gw_put() { # <ws> <path> <body> -> http code on stdout, body in /tmp/gw.body
  $K -n flint-system exec -i gwcurl -- sh -c \
    "curl -sS -o /tmp/gw.body -w '%{http_code}' -X PUT \
       -H 'Authorization: Bearer $GWTOKEN' --data-binary @- \
       'http://$GW_SVC:$GW_PORT/lean/v1/$1/files/$2'" <<< "$3" 2>/dev/null | tr -dc '0-9'
}
gw_body() { $K -n flint-system exec gwcurl -- cat /tmp/gw.body 2>/dev/null; }

# ── pod helpers ──────────────────────────────────────────────────────
inpod()  { $K -n $NS exec "$1" -c agent -- /bin/sh -c "$2" 2>/dev/null; }
tmpf()   { inpod "$1" "cat /tmp/$2 2>/dev/null" | tr -d '\r\n '; }
jpath()  { $K -n $NS get pod "$1" -o jsonpath="$2" 2>/dev/null; }

# Every leg that reads a pod asserts the pod EXISTS and is Running
# first. Without this an absent pod makes each observation an empty
# string, and empty strings satisfy "the file is not there", "no
# credentials are set" and "nothing leaked" — three of this rig's own
# assertions pass on a pod that was never created. Observed, not
# theorised: a run from the wrong directory scored A6 green twice
# against nothing.
require_pod() {
  local ph
  ph=$($K -n $NS get pod "$1" -o jsonpath='{.status.phase}' 2>/dev/null)
  [ "$ph" = "Running" ] && return 0
  bad "pod $1 is '${ph:-absent}', not Running — every observation this leg makes would be an empty string"
  return 1
}

# Wait for a file in the agent's tree to satisfy a grep, or time out.
wait_in() { # <pod> <path> <pattern> <iters>
  local i=0
  while [ $i -lt "$4" ]; do
    inpod "$1" "grep -c '$3' '$2'" > /dev/null 2>&1 && return 0
    i=$((i + 1)); sleep 1
  done
  return 1
}

# Apply a manifest from stdin and report ADMITTED / DENIED plus the
# API server's message. Never `|| true` — the point is the message.
apply_expect_denied() { # <name-for-report>  (manifest on stdin)
  local out rc
  out=$($K apply -f - 2>&1); rc=$?
  DENY_MSG=$out
  [ $rc -ne 0 ]
}

# ─────────────────────────────────────────────────────────────────────
if [ "${1:-}" = "setup" ]; then
  $K apply -f minio.yaml
  $K -n flint-system wait --for=condition=Available deploy/minio --timeout=180s
  $K -n flint-system delete pod mc-agent --ignore-not-found > /dev/null
  $K -n flint-system run mc-agent --image=minio/mc --restart=Never \
     --command -- sleep 100000 > /dev/null
  $K -n flint-system wait --for=condition=Ready pod/mc-agent --timeout=180s
  mcx mc alias set m http://minio.flint-system.svc:9000 drill drillsecret
  mcx mc mb --ignore-existing "m/$BUCKET"
  $K -n flint-system delete pod gwcurl --ignore-not-found > /dev/null
  $K -n flint-system run gwcurl --image=curlimages/curl --restart=Never \
     --command -- sleep 100000 > /dev/null
  $K -n flint-system wait --for=condition=Ready pod/gwcurl --timeout=180s
  $K apply -f agent-pod.yaml
  echo "setup done: bucket $BUCKET, workspaces proj + other in ns $NS"
  exit 0
fi

for f in agent-pod.yaml pod-seeder.yaml pod-agent1.yaml pod-other.yaml; do
  [ -f "$f" ] || { echo "FAIL: $f is not beside this script (cwd $(pwd))"; exit 1; }
done

echo "agent-pod use case  (context $CTX)"

# The rig's own preconditions. A missing mc pod or an absent workspace
# would otherwise turn every store assertion below into a silent empty
# string, and empty strings compare equal to each other.
$K -n flint-system get pod mc-agent > /dev/null 2>&1 || {
  echo "FAIL: no mc-agent pod — run './run-agent.sh setup' first"; exit 1; }
mcx mc alias set m http://minio.flint-system.svc:9000 drill drillsecret > /dev/null
mcx mc ls "m/$BUCKET" > /dev/null 2>&1 || {
  echo "FAIL: bucket $BUCKET not reachable from the cluster"; exit 1; }
$K -n flint-system get pod gwcurl > /dev/null 2>&1 || {
  echo "FAIL: no gwcurl pod — run './run-agent.sh setup' first"; exit 1; }
[ "$($K -n $NS get flintleanworkspace proj -o jsonpath='{.spec.floorSecs}')" = "3600" ] || {
  echo "FAIL: workspace proj does not carry the hour-long floor this rig's"
  echo "      anti-vacuity depends on — every boundary leg would be creditable"
  echo "      to a cadence tick"; exit 1; }

# ─────────────────────────────────────────────────────────────────────
# A1  A job-shaped agent seeds the project and DECLARES its boundary.
#     The floor is an hour away, so the only thing that can have
#     published this is the agent's own `.flint/publish`.
# ─────────────────────────────────────────────────────────────────────
a1_seed() {
  $K -n $NS delete pod seeder --ignore-not-found --grace-period=1 > /dev/null
  $K apply -f pod-seeder.yaml > /dev/null || { bad "seeder rejected at admission"; return 1; }
  local i=0 phase=""
  while [ $i -lt 120 ]; do
    phase=$($K -n $NS get pod seeder -o jsonpath='{.status.phase}' 2>/dev/null)
    [ "$phase" = "Succeeded" ] && break
    [ "$phase" = "Failed" ] && break
    i=$((i + 1)); sleep 2
  done
  [ "$phase" = "Succeeded" ] || {
    bad "seeder ended $phase: $($K -n $NS logs seeder -c agent --tail=5 2>&1)"; return 1; }
  $K -n $NS logs seeder -c agent 2>/dev/null | grep -c "SEED PUBLISHED" > /dev/null \
    || { bad "seeder exited 0 without ever reading its ack"; return 1; }
  ok "the seeding agent declared a boundary and waited for the answer"

  local n
  n=$(objcount tenants/proj/files/src/)
  [ "$n" = "200" ] || { bad "the bucket holds $n objects under files/src/, not 200"; return 1; }
  mhas tenants/proj "src/f0042.txt" || { bad "the manifest does not cite src/f0042.txt"; return 1; }
  ok "200 objects published and cited at seq $(mseq tenants/proj) — no tick could have done it"
}

# ─────────────────────────────────────────────────────────────────────
# A2  THE HEADLINE: a DIFFERENT pod, created after the first was gone,
#     finds the whole project already in its filesystem before its own
#     first instruction. This is what an agent gets for one label.
# ─────────────────────────────────────────────────────────────────────
a2_cold_agent_gets_the_project() {
  $K -n $NS delete pod seeder --grace-period=1 --wait=true > /dev/null 2>&1
  $K -n $NS get pod seeder > /dev/null 2>&1 && { bad "the seeder pod still exists"; return 1; }
  ok "the pod that wrote the project is gone; only bucket objects remain"

  $K -n $NS delete pod agent-1 --ignore-not-found --grace-period=1 > /dev/null
  $K apply -f pod-agent1.yaml > /dev/null || { bad "agent-1 rejected at admission"; return 1; }
  $K -n $NS wait --for=condition=Ready pod/agent-1 --timeout=300s > /dev/null 2>&1 || {
    bad "agent-1 never became Ready: $($K -n $NS describe pod agent-1 | tail -8)"; return 1; }

  [ "$(tmpf agent-1 gate)" = "GATE-OK" ] || {
    bad "the agent's first instruction ran before checkout-complete existed"; return 1; }
  local seen sample
  seen=$(tmpf agent-1 seen-count)
  [ "$seen" = "200" ] || {
    bad "the agent's FIRST instruction saw $seen files, not 200 — the start gate let it run against a partial tree"
    return 1; }
  sample=$(inpod agent-1 "cat /tmp/seen-sample")
  echo "$sample" | grep -c "unit 0042 of the seeded project" > /dev/null || {
    bad "f0042.txt read back as '$sample'"; return 1; }
  ok "a pod that never spoke S3 opened all 200 files, correct bytes, before its first instruction"
}

# ─────────────────────────────────────────────────────────────────────
# A3  The pod spec names a workspace and nothing else. Asserted against
#     the AUTHOR'S file and the LIVE spec together: absent there,
#     present here, so the operator is demonstrably the one supplying
#     it. Either half alone is vacuous.
# ─────────────────────────────────────────────────────────────────────
a3_the_pod_declares_a_name_not_an_endpoint() {
  require_pod agent-1 || return 1
  # STRUCTURAL, not a text match. The first version of this leg grepped
  # the file for "AWS_" and went red on the agent's own probe command
  # (`env | grep -c '^AWS_'`) — a text scan cannot tell an assertion
  # from a configuration. What matters is what the author DECLARES, so
  # read the spec as a document: zero volumes, zero initContainers,
  # zero env, zero envFrom, zero volumeMounts, and one label.
  local verdict
  verdict=$(python3 - pod-agent1.yaml <<'EOF'
import sys, yaml
d = yaml.safe_load(open(sys.argv[1]))
spec, meta = d["spec"], d["metadata"]
declared = {
    "volumes":        spec.get("volumes") or [],
    "initContainers": spec.get("initContainers") or [],
    "env":            [e for c in spec["containers"] for e in (c.get("env") or [])],
    "envFrom":        [e for c in spec["containers"] for e in (c.get("envFrom") or [])],
    "volumeMounts":   [m for c in spec["containers"] for m in (c.get("volumeMounts") or [])],
}
nonempty = {k: len(v) for k, v in declared.items() if v}
label = (meta.get("labels") or {}).get("chert.us/lean-workspace")
if nonempty:
    print("DECLARES " + ", ".join(f"{k}={n}" for k, n in nonempty.items()))
elif label != "proj":
    print(f"LABEL {label!r}")
else:
    print("CLEAN")
EOF
)
  [ "$verdict" = "CLEAN" ] || {
    bad "the author's spec is not name-only: $verdict"; return 1; }
  ok "the author's spec declares one label and nothing else: no volume, mount, env, envFrom or sidecar"

  [ "$(jpath agent-1 '{.spec.initContainers[0].name}/{.spec.initContainers[0].restartPolicy}')" \
      = "flint-sync/Always" ] || { bad "no native sidecar in the live spec"; return 1; }
  [ "$(jpath agent-1 '{.spec.containers[0].volumeMounts[?(@.name=="flint-workspace")].mountPath}')" \
      = "/workspace" ] || { bad "the agent container does not mount the workspace"; return 1; }
  local ep grace
  ep=$(jpath agent-1 '{.spec.initContainers[0].env[?(@.name=="FLINT_SYNC_ENDPOINT")].value}')
  [ "$ep" = "http://minio.flint-system.svc:9000" ] || {
    bad "the sidecar's endpoint is '$ep'"; return 1; }
  grace=$(jpath agent-1 '{.spec.terminationGracePeriodSeconds}')
  [ -n "$grace" ] && [ "$grace" -gt 30 ] || {
    bad "grace is '$grace' — the drain was left in the 30 s default nobody chose"; return 1; }
  ok "the live spec carries all of it: sidecar, mount, endpoint, and a ${grace}s derived grace"
  # Surfaced, not asserted: for cadence/hybrid the derivation is
  # floorSecs + retry + slack, so a long RPO buys a long CEILING. It
  # costs nothing on a spot reclaim (the node is gone at the notice
  # either way) but it is the whole wait on a `kubectl delete` or a
  # rollout, and native-sidecar ordering spends the AGENT's share of it
  # first — a container whose PID 1 is a shell running `sleep` never
  # sees SIGTERM and sits out the entire budget before the drain is
  # asked for anything. Both agent pods here trap TERM for that reason.
  [ "$grace" -le 300 ] || note "the derived grace is ${grace}s (floorSecs=$($K -n $NS get flintleanworkspace proj -o jsonpath='{.spec.floorSecs}') + retry + slack); an agent that ignores SIGTERM burns all of it before the sidecar drains"
}

# ─────────────────────────────────────────────────────────────────────
# A4  No credential reaches the agent container — checked in the
#     RUNNING process's environment, not just the spec. The sidecar's
#     own envFrom is asserted non-empty in the same breath, or "nobody
#     has credentials" would pass this leg.
# ─────────────────────────────────────────────────────────────────────
a4_credentials_stop_at_the_sidecar() {
  require_pod agent-1 || return 1
  local awsn flintn efrom
  awsn=$(tmpf agent-1 aws-count)
  flintn=$(tmpf agent-1 flint-count)
  [ "$awsn" = "0" ] || { bad "the agent's environment carries $awsn AWS_* variables"; return 1; }
  [ "$flintn" = "0" ] || { bad "the agent's environment carries $flintn FLINT_SYNC_* variables"; return 1; }
  [ -z "$(jpath agent-1 '{.spec.containers[0].envFrom}')" ] || {
    bad "the agent container has an envFrom"; return 1; }
  efrom=$(jpath agent-1 '{.spec.initContainers[0].envFrom[0].secretRef.name}')
  [ "$efrom" = "lean-proxy-creds" ] || {
    bad "the sidecar's envFrom is '$efrom' — if nothing has credentials this leg proves nothing"
    return 1; }
  ok "0 AWS_* and 0 FLINT_SYNC_* in the live agent process; the sidecar holds $efrom alone"
}

# ─────────────────────────────────────────────────────────────────────
# A5  The agent declares a boundary from inside its own container and
#     is answered. Anti-vacuity is structural: the floor is an hour.
# ─────────────────────────────────────────────────────────────────────
a5_the_agent_declares_a_boundary() {
  require_pod agent-1 || return 1
  local s0 s1 ack
  s0=$(mseq tenants/proj)
  inpod agent-1 "printf 'the agent reports\n' > /workspace/report.md" > /dev/null
  inpod agent-1 "mkdir -p /workspace/.flint; printf '{\"nonce\":\"a5-n1\"}' > /workspace/.flint/publish.tmp; mv /workspace/.flint/publish.tmp /workspace/.flint/publish" > /dev/null
  wait_in agent-1 /workspace/.flint/publish.ack a5-n1 90 || {
    bad "no ack covering a5-n1 after 90 s: $(inpod agent-1 'cat /workspace/.flint/publish.ack')"
    return 1; }
  ack=$(inpod agent-1 "cat /workspace/.flint/publish.ack")
  echo "$ack" | grep -c '"status": *"ok"' > /dev/null || {
    bad "the ack status is not ok: $ack"; return 1; }
  s1=$(mseq tenants/proj)
  [ -n "$s1" ] && [ "$s1" -gt "$s0" ] || {
    bad "the ack landed but the manifest did not advance ($s0 -> $s1)"; return 1; }
  [ "$(objcat tenants/proj/files/report.md)" = "the agent reports" ] || {
    bad "the acked bytes are not in the bucket"; return 1; }
  ok "declared boundary answered ok: seq $s0 -> $s1, report.md durable and cited"
}

# ─────────────────────────────────────────────────────────────────────
# A6  Somebody else edits the project — a human, a CI job, another
#     tool — through the gateway, and the running agent picks it up on
#     `.flint/sync`. Asserted ABSENT first, or "it was always there"
#     passes the leg.
# ─────────────────────────────────────────────────────────────────────
a6_a_foreign_edit_reaches_the_running_agent() {
  require_pod agent-1 || return 1
  inpod agent-1 "test -f /workspace/notes/human.md" && {
    bad "notes/human.md was already in the tree before the foreign write"; return 1; }
  ok "precondition: the path is absent from the agent's tree"

  # 409 barrier-window-open is the DOCUMENTED retryable answer (a
  # publish barrier holds the window), so retry it and fail on anything
  # else — swallowing every non-200 would make this leg unfalsifiable.
  local code i=0
  while [ $i -lt 30 ]; do
    code=$(gw_put proj notes/human.md 'a human edited this')
    [ "$code" = "409" ] || break
    i=$((i + 1)); sleep 2
  done
  [ "$code" = "200" ] || { bad "the gateway PUT returned '$code': $(gw_body)"; return 1; }
  ok "the gateway accepted the foreign write (object + inbox entry, no manifest edit)"

  inpod agent-1 "mkdir -p /workspace/.flint; printf '{\"nonce\":\"a6-n1\"}' > /workspace/.flint/sync.tmp; mv /workspace/.flint/sync.tmp /workspace/.flint/sync" > /dev/null
  wait_in agent-1 /workspace/.flint/sync.ack a6-n1 90 || {
    bad "no sync ack covering a6-n1: $(inpod agent-1 'cat /workspace/.flint/sync.ack')"; return 1; }
  local body
  body=$(inpod agent-1 "cat /workspace/notes/human.md")
  [ "$body" = "a human edited this" ] || {
    bad "the agent's tree holds '$body' after the sync"; return 1; }
  ok "the foreign edit reached the running agent's filesystem on a declared sync"
}

# ─────────────────────────────────────────────────────────────────────
# A7  Two teams, one bucket. Each agent sees its own subtree and only
#     its own — asserted in BOTH directions.
# ─────────────────────────────────────────────────────────────────────
a7_tenancy() {
  $K -n $NS delete pod agent-other --ignore-not-found --grace-period=1 > /dev/null
  $K apply -f pod-other.yaml > /dev/null || { bad "agent-other rejected at admission"; return 1; }
  $K -n $NS wait --for=condition=Ready pod/agent-other --timeout=300s > /dev/null 2>&1 || {
    bad "agent-other never became Ready"; return 1; }
  require_pod agent-other || return 1
  [ "$(tmpf agent-other seen-count)" = "0" ] || {
    bad "team-b's agent sees $(tmpf agent-other seen-count) files from team-a's src/"; return 1; }
  [ -z "$(inpod agent-other 'cat /tmp/leak')" ] || {
    bad "team-b's agent found $(inpod agent-other 'cat /tmp/leak')"; return 1; }
  inpod agent-other "mkdir -p /workspace/.flint; printf '{\"nonce\":\"a7-n1\"}' > /workspace/.flint/publish.tmp; mv /workspace/.flint/publish.tmp /workspace/.flint/publish" > /dev/null
  wait_in agent-other /workspace/.flint/publish.ack a7-n1 90 || {
    bad "team-b's boundary never acked"; return 1; }
  [ "$(objcat tenants/other/files/theirs.txt)" = "team-b work" ] || {
    bad "team-b's write is not under its own prefix"; return 1; }
  inpod agent-1 "test -f /workspace/theirs.txt" && {
    bad "team-b's file appeared in team-a's tree"; return 1; }
  ok "each subtree is invisible to the other, and team-b's publish landed under its own prefix"
}

# ─────────────────────────────────────────────────────────────────────
# A8  CONTROL: a pod naming a workspace that does not exist must not
#     schedule. With the ACCEPTED control beside it — the identical pod
#     WITH a real label is admitted — so this cannot be passing because
#     admission rejects everything.
# ─────────────────────────────────────────────────────────────────────
a8_missing_workspace_is_refused() {
  $K -n $NS delete pod ghost accepted-control --ignore-not-found > /dev/null 2>&1
  if apply_expect_denied <<EOF
apiVersion: v1
kind: Pod
metadata: { name: ghost, namespace: $NS, labels: { chert.us/lean-workspace: no-such-ws } }
spec:
  restartPolicy: Never
  containers: [{ name: agent, image: busybox:stable, command: ["sleep","30"] }]
EOF
  then
    echo "$DENY_MSG" | grep -c "no-such-ws" > /dev/null || {
      bad "denied, but the message never names the workspace: $DENY_MSG"; return 1; }
    ok "a pod naming a missing workspace is denied, and the message names it"
  else
    bad "a pod naming a MISSING workspace was ADMITTED — the gate is vacuous"; return 1
  fi

  if apply_expect_denied <<EOF
apiVersion: v1
kind: Pod
metadata: { name: accepted-control, namespace: $NS, labels: { chert.us/lean-workspace: other } }
spec:
  restartPolicy: Never
  containers: [{ name: agent, image: busybox:stable, command: ["sleep","30"] }]
EOF
  then
    bad "the ACCEPTED control was denied too — admission is refusing everything: $DENY_MSG"; return 1
  fi
  $K -n $NS delete pod accepted-control --ignore-not-found > /dev/null 2>&1
  ok "accepted control: the same pod with a real workspace is admitted"
}

# ─────────────────────────────────────────────────────────────────────
# A9  CONTROL: a pod that already uses the workspace path is refused
#     with a message that names the knob. The alternative — silently
#     skipping the mount — would leave the agent running against its
#     own empty volume while every probe stayed green.
# ─────────────────────────────────────────────────────────────────────
a9_path_collision_is_refused_actionably() {
  $K -n $NS delete pod clash --ignore-not-found > /dev/null 2>&1
  if apply_expect_denied <<EOF
apiVersion: v1
kind: Pod
metadata: { name: clash, namespace: $NS, labels: { chert.us/lean-workspace: other } }
spec:
  restartPolicy: Never
  volumes: [{ name: mine, emptyDir: {} }]
  containers:
    - name: agent
      image: busybox:stable
      command: ["sleep","30"]
      volumeMounts: [{ name: mine, mountPath: /workspace }]
EOF
  then
    echo "$DENY_MSG" | grep -c "mountPath" > /dev/null || {
      bad "denied without naming the knob that fixes it: $DENY_MSG"; return 1; }
    ok "a pod that already owns /workspace is refused, naming spec.mountPath"
  else
    bad "a pod whose own volume shadows the workspace was ADMITTED"; return 1
  fi
}

# ─────────────────────────────────────────────────────────────────────
run_leg() { leg "$1"; shift; "$@" && echo "  PASS" || echo "  FAIL"; }

run_leg "A1 a job-shaped agent seeds the project at a declared boundary" a1_seed
run_leg "A2 a cold agent finds the whole project already in its filesystem" a2_cold_agent_gets_the_project
run_leg "A3 the pod declares a name, the operator supplies the rest" a3_the_pod_declares_a_name_not_an_endpoint
run_leg "A4 credentials stop at the sidecar" a4_credentials_stop_at_the_sidecar
run_leg "A5 the agent declares a boundary and is answered" a5_the_agent_declares_a_boundary
run_leg "A6 a foreign edit reaches the running agent" a6_a_foreign_edit_reaches_the_running_agent
run_leg "A7 two teams, one bucket, no leakage" a7_tenancy
run_leg "A8 CONTROL a missing workspace is refused" a8_missing_workspace_is_refused
run_leg "A9 CONTROL a path collision is refused actionably" a9_path_collision_is_refused_actionably

echo
echo "agent-pod use case: $PASS assertions green, $FAILED bad"
[ "$FAILED" -eq 0 ]
