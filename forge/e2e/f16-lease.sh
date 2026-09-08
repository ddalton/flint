#!/usr/bin/env bash
# F16 — THE LEASE: exactly one writer, on the wire.
#
#   BUCKET=... PREFIX=... ./forge/e2e/f16-lease.sh
#
# WHY THIS DRILL EXISTS. "Exactly one writer" is forge's central safety
# claim and, until this script, the only core claim with no cluster
# drill: `lease` appears in no other drill in this directory. It is
# unit-tested and it is modelled — which is exactly the pairing that
# missed the defect F14 found on runcj in a single run. And this area
# has a defect history that neither method caught first: the takeover
# rotation had two gaps found by a model, a fold commit did not
# revalidate the lease, and a live lean sidecar once fenced ITSELF into
# silence by reading a lost renewal response as a deposal.
#
# It also exercises the one S3 behaviour no local rig can: the lease is
# `If-Match`/`If-None-Match: *` on the ETag of `<prefix>/git/epoch`.
# `gitqual` qualifies the git protocol against MinIO; conditional PUT is
# where MinIO and S3 are least alike, so it has never been qualified
# against the real thing.
#
# THE SHAPE IS A BICONDITIONAL, and both arms must run.
#
#   P1  a LIVE holder is NOT superseded          (takeover must not happen)
#   P3  a QUIET holder IS superseded             (takeover must happen)
#
# Either alone is worthless. A challenger that crashed on startup passes
# P1; a lease that never holds anything passes P3. If only one arm
# produces its result the run is INCONCLUSIVE, not a pass.
#
# TWO INDEPENDENT OBSERVATIONS, NEVER ONE. Every claim about who holds
# the lease is read BOTH from the pod's own `/status` and from the
# bucket's `<prefix>/git/epoch`, and they must agree. A server's opinion
# of whether it is the writer is exactly the sensor that lies in a
# split-brain, so it is a state variable here and not the truth.
#
# THE CHALLENGER IS DERIVED, NOT WRITTEN. It is the live Deployment with
# a new name and one added label, so it differs from the holder in
# identity and in nothing else. A hand-copied pod spec would drift from
# what the operator renders and the drill would stop testing the shipped
# thing.
#
# THE ZOMBIE LEG IS THE POINT (P4), AND ITS VACUITY TRAP IS THE REASON
# IT IS SHAPED THIS WAY. After the deposed holder gets S3 back, "it did
# not write" is worth nothing unless it TRIED. So a commit carrying a
# unique marker is pushed directly into the deposed pod, bypassing the
# door and the Service through `kubectl port-forward` — the shape of a
# client whose connection survived the partition. That marker must never
# appear in the bucket. A leg that merely watched an idle pod not write
# is reported INCONCLUSIVE.
#
# NOTHING HERE IS TIMED BY A WALL CLOCK IT ASSUMES. The term is read
# from `/status.epoch.termSecs` and every wait is derived from it, so a
# cluster running a different heartbeat does not silently turn a
# takeover window into a pass.
set -uo pipefail
NS=${NS:-agents}
REPO=${REPO:-f16}
: "${BUCKET:?set BUCKET}"; : "${PREFIX:?set PREFIX}"
DOOR=${DOOR:-http://flint-forge-door.forge-system.svc}
AGENT=${AGENT:-f16agent}
TAG=${TAG:?set TAG to the image tag the rig deployed}
HERE=$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)
PASS=0; FAIL=0; INC=0
RUN=$$
M_P0="f16-p0-$RUN"; M_P1="f16-p1-$RUN"; M_DIRECT="f16-direct-$RUN"
CHAL="forge-$REPO-chal"
NP=f16-partition
K() { kubectl "$@"; }
ok()   { PASS=$((PASS+1)); printf '  PASS  %s\n' "$*"; }
bad()  { FAIL=$((FAIL+1)); printf '  FAIL  %s\n' "$*"; }
inc()  { INC=$((INC+1));  printf '  ????  INCONCLUSIVE — %s\n' "$*"; }
note() { printf '  ....  %s\n' "$*"; }
hdr()  { printf '\n== %s ==\n' "$*"; }

cleanup() {
  pkill -f "port-forward -n $NS pod/forge-$REPO" 2>/dev/null
  K delete -n "$NS" networkpolicy "$NP" --wait=false >/dev/null 2>&1
  K delete -n "$NS" deploy "$CHAL" --wait=false >/dev/null 2>&1
  K delete -n "$NS" pod "$AGENT" --ignore-not-found >/dev/null 2>&1
  for p in $(K get pods -n "$NS" -o name 2>/dev/null | grep "forge-$REPO"); do
    K label -n "$NS" "$p" chaos- >/dev/null 2>&1
  done
}
trap cleanup EXIT

# ── observation ──────────────────────────────────────────────────────
# Every pod of this repository, holder and challenger alike.
repo_pods() { K get pods -n "$NS" -o name 2>/dev/null | grep "forge-$REPO" | sed 's|pod/||'; }
status_port() {
  K -n "$NS" get deploy "forge-$REPO" \
    -o jsonpath='{.spec.template.spec.containers[?(@.name=="syncer")].ports[?(@.name=="status")].containerPort}' 2>/dev/null
}
# The `git` port is on the git-http container, NOT the syncer — only
# `status` and `files` are the syncer's. Reading it from the wrong
# container returns empty, and an empty port silently turns P4's
# port-forward into a leg that could never reach anything.
git_port() {
  K -n "$NS" get deploy "forge-$REPO" \
    -o jsonpath='{.spec.template.spec.containers[?(@.name=="git-http")].ports[?(@.name=="git")].containerPort}' 2>/dev/null
}
# `/status` of ONE named pod. Never `deploy/...`: with two pods that
# picks one arbitrarily, which is precisely the ambiguity under test.
pod_status() {
  [ -n "${SPORT:-}" ] || return 1
  K -n "$NS" exec "$1" -c syncer -- wget -qO- "http://127.0.0.1:$SPORT/status" 2>/dev/null
}
sfield() { pod_status "$1" | jq -r "$2 // \"\"" 2>/dev/null; }
# Booleans and `fenced` must NOT go through `// ""`: jq treats both
# `false` and `null` as absent, so a real `false` would be reported as
# an empty read and the two would be indistinguishable in a message.
sraw()   { pod_status "$1" | jq -r "$2" 2>/dev/null; }
# Is this pod up and writing? NOT `phase == serving`: the phase strings
# are lowercase (`Phase::as_str`) and the steady state moves through
# `pushing` and `sweeping` while perfectly healthy, so an exact match
# samples a race. What actually means "this is the writer" is the pair
# the code's own `serving()` rests on — the lease is held and nothing
# has fenced it.
is_up() {
  local ph
  ph=$(sraw "$1" .phase)
  case "$ph" in serving|pushing|sweeping) ;; *) return 1 ;; esac
  [ "$(sraw "$1" '.epoch.held')" = true ] || return 1
  [ "$(sraw "$1" .fenced)" = null ]
}

# The bucket's own answer. Discovered, never constructed: `keyPrefix`
# and the repo path compose in a way that has been got wrong by hand
# before, and a key that does not exist reads as "no lease" — an error
# returning a legal value.
find_epoch_key() {
  local n
  # Scoped to THIS repository's own keyPrefix, not to $PREFIX: the rig
  # deploys `proj` at `<prefix>/git/` in the same bucket, so a search
  # from `<prefix>/` finds two leases and the drill would be observing
  # another repository's writer. The "exactly one" guard caught this on
  # the first run rather than silently picking the wrong one.
  EPOCH_KEY=$(aws s3 ls "s3://$BUCKET/$PREFIX/$REPO/" --recursive 2>/dev/null | awk '{print $4}' | grep '/git/epoch$')
  n=$(printf '%s\n' "$EPOCH_KEY" | grep -c . )
  [ "${n:-0}" = 1 ] || { EPOCH_KEY=""; return 1; }
}
bucket_epoch() { aws s3 cp "s3://$BUCKET/$EPOCH_KEY" - 2>/dev/null; }
bucket_holder() { bucket_epoch | jq -r '.holder_id // ""' 2>/dev/null; }
bucket_epochno() { bucket_epoch | jq -r '.epoch // ""' 2>/dev/null; }

# Which pod the BUCKET says is the writer. The join between the two
# observations, and what every "who holds it" assertion below rests on.
holder_pod() {
  local want p
  want=$(bucket_holder); [ -n "$want" ] || return 1
  for p in $(repo_pods); do
    [ "$(sfield "$p" .serverId)" = "$want" ] && { printf '%s' "$p"; return 0; }
  done
  return 1
}

# ── the client ───────────────────────────────────────────────────────
# One push through the door, from the agent pod. $1 = marker; the
# marker is the content, so a commit can be traced into the bucket.
agent_push() {
  K exec -n "$NS" "$AGENT" -- sh -c "
    set -e
    T=\$(cat /var/run/secrets/forge/token)
    H=\"Authorization: Basic \$(printf 'x:%s' \"\$T\" | base64 | tr -d '\n')\"
    rm -rf /tmp/w && git -c http.$DOOR/.extraHeader=\"\$H\" clone -q -b agents $DOOR/git/$NS/$REPO.git /tmp/w 2>/dev/null || {
      mkdir -p /tmp/w && cd /tmp/w && git init -q -b agents . && git remote add origin $DOOR/git/$NS/$REPO.git; }
    cd /tmp/w
    git config user.email a@b.c; git config user.name a
    echo '$1' >> marks.txt
    git add marks.txt && git commit -q -m '$1'
    git -c http.$DOOR/.extraHeader=\"\$H\" push -q origin agents
  " 2>&1
}
# A push straight into ONE named pod, bypassing the door and the
# Service through `kubectl port-forward` — the shape of a client whose
# connection survived a partition. It CLONES FROM THAT POD FIRST, so
# what it offers is a fast-forward on that pod's own tip: a healthy
# holder accepts it. That is the whole point. P4's first draft pushed
# an unrelated history from `git init`, which any holder would refuse
# on staleness alone — the leg would have passed while proving nothing
# about the lease.
#   $1 = pod, $2 = marker. Sets DPUSH_OUT and DPUSH_RC.
LPORT=18716
direct_push() {
  local pod marker d pf
  pod=$1; marker=$2
  LPORT=$((LPORT + 1))
  K port-forward -n "$NS" "pod/$pod" "$LPORT:$GPORT" >/dev/null 2>&1 &
  pf=$!
  sleep 4
  d=$(mktemp -d)
  DPUSH_OUT=$( cd "$d" \
    && git -c http.extraHeader="X-Remote-User: agent-runner" \
           clone -q -b agents "http://127.0.0.1:$LPORT/$NS/$REPO.git" w 2>&1 \
    && cd w \
    && git config user.email z@z.z && git config user.name z \
    && echo "$marker" >> marks.txt && git add marks.txt && git commit -q -m "$marker" \
    && git -c http.extraHeader="X-Remote-User: agent-runner" push origin agents 2>&1 )
  DPUSH_RC=$?
  kill "$pf" 2>/dev/null
  rm -rf "$d"
}

# `grep -c` PRINTS 0 and EXITS 1, so `grep -c x f || echo 0` yields
# "0\n0" and every arithmetic test downstream is garbage. F14 lost a
# whole run to this.
count() { local n; n=$(printf '%s\n' "$1" | grep -c "$2" 2>/dev/null); printf '%s' "${n:-0}"; }

echo "F16 — the lease, on the wire.  repo=$REPO ns=$NS"

# ── P0: preconditions, and they must be real ─────────────────────────
hdr "P0: one writer, and two observations that agree"
sed -e "s|__REPO__|$REPO|g" -e "s|__NS__|$NS|g" -e "s|__BUCKET__|$BUCKET|g" \
    -e "s|__PREFIX__|$PREFIX|g" "$HERE/f16-repo.yaml.tpl" | K apply -f - >/dev/null
# The SHARED agent template, not one of this drill's own: an agent is
# one projected token and nothing else, and a drill that rolled its own
# could give itself authority the design does not grant.
# A Pod is not a Deployment: `apply` over one that is still Terminating
# from a previous run's cleanup FAILS, and the first draft sent that
# error to /dev/null and then reported the consequence — "the snapshot
# did not advance" — as though the REPOSITORY had not written. A missing
# client must never read as a broken server.
K delete -n "$NS" pod "$AGENT" --ignore-not-found >/dev/null 2>&1
for _ in $(seq 1 24); do
  K get -n "$NS" pod "$AGENT" >/dev/null 2>&1 || break
  sleep 5
done
APPLY=$(AGENT=$AGENT TAG=$TAG envsubst '$AGENT $TAG' < "$HERE/agent.yaml.tpl" | K apply -f - 2>&1)
for _ in $(seq 1 24); do
  [ "$(K get -n "$NS" pod "$AGENT" -o jsonpath='{.status.phase}' 2>/dev/null)" = Running ] && break
  sleep 5
done
[ "$(K get -n "$NS" pod "$AGENT" -o jsonpath='{.status.phase}' 2>/dev/null)" = Running ] \
  && ok "the agent pod is Running" \
  || { bad "no agent pod — every push leg below would report a missing CLIENT as a stalled SERVER: $APPLY"; exit 1; }

for _ in $(seq 1 60); do
  [ "$(K get -n "$NS" flintrepo "$REPO" -o jsonpath='{.status.phase}' 2>/dev/null)" = Ready ] && break
  sleep 5
done
SPORT=$(status_port); GPORT=$(git_port)
[ -n "$SPORT" ] && ok "the status port is $SPORT" || { bad "no status port — nothing below can observe anything"; exit 1; }
[ -n "$GPORT" ] && ok "the git port is $GPORT" || bad "no git port on the git-http container — P4 cannot reach the deposed pod"

H0=$(repo_pods | head -1)
[ -n "$H0" ] || { bad "no pod for forge-$REPO"; exit 1; }
is_up "$H0" && ok "the holder is up and holds the lease (phase=$(sraw "$H0" .phase))" \
  || { bad "the holder is not writing: phase=$(sraw "$H0" .phase) held=$(sraw "$H0" '.epoch.held') fenced=$(sraw "$H0" .fenced)"; exit 1; }

find_epoch_key && ok "the lease object is s3://$BUCKET/$EPOCH_KEY" \
  || { bad "found $(aws s3 ls "s3://$BUCKET/$PREFIX/$REPO/" --recursive 2>/dev/null | awk '{print $4}' | grep -c '/git/epoch$') lease objects under $PREFIX/$REPO/, not one — cannot observe the lease independently"; exit 1; }

ID0=$(sfield "$H0" .serverId); BID0=$(bucket_holder); E0=$(bucket_epochno)
TERM=$(sfield "$H0" '.epoch.termSecs'); TERM=${TERM:-60}
note "holder=$ID0 epoch=$E0 term=${TERM}s"
[ -n "$ID0" ] && [ "$ID0" = "$BID0" ] \
  && ok "the pod and the bucket name the same holder" \
  || bad "the pod says '$ID0' and the bucket says '$BID0' — they already disagree"
[ "$(sraw "$H0" '.epoch.held')" = true ] && ok "and the holder says it holds the lease" \
  || bad "the holder does not claim the lease"

# A writer that is not writing makes every "nothing was written" below
# vacuous. Prove the ordinary path works before breaking it.
SEQ0=$(sfield "$H0" '.repo.snapshotSeq')
PUSHOUT=$(agent_push "$M_P0" 2>&1)
sleep 5
SEQ1=$(sfield "$H0" '.repo.snapshotSeq')
[ -n "$SEQ1" ] && [ "${SEQ1:-0}" -gt "${SEQ0:-0}" ] \
  && ok "a push advances the snapshot ($SEQ0 -> $SEQ1) — the writer is writing" \
  || { bad "the snapshot did not advance ($SEQ0 -> $SEQ1); later legs would prove nothing"
       note "the client said: $(printf '%s' "$PUSHOUT" | tr '\n' ' ' | cut -c1-200)"; }

# ── P1: a LIVE holder is not superseded ──────────────────────────────
hdr "P1: a second syncer does not take a lease that is being renewed"
K get -n "$NS" deploy "forge-$REPO" -o json 2>/dev/null \
 | jq --arg n "$CHAL" '
     .metadata.name = $n
   | del(.metadata.uid, .metadata.resourceVersion, .metadata.creationTimestamp,
         .metadata.generation, .metadata.ownerReferences, .metadata.annotations, .status)
   | .spec.replicas = 1
   | .spec.selector.matchLabels["f16-role"] = "challenger"
   | .spec.template.metadata.labels["f16-role"] = "challenger"
   ' 2>/dev/null | K apply -f - >/dev/null 2>&1 \
 && ok "the challenger is derived from the live Deployment, not hand-written" \
 || { bad "could not derive the challenger"; exit 1; }

CP=""
for _ in $(seq 1 24); do
  CP=$(K get pods -n "$NS" -l f16-role=challenger -o name 2>/dev/null | sed 's|pod/||' | head -1)
  [ -n "$CP" ] && [ "$(K get -n "$NS" pod "$CP" -o jsonpath='{.status.phase}' 2>/dev/null)" = Running ] && break
  sleep 5
done
[ -n "$CP" ] && ok "the challenger pod is $CP" || { bad "the challenger never started"; exit 1; }

# THE LOAD-BEARING CONTROL for this leg. A challenger that died is not
# evidence that the lease held it off.
sleep $((TERM + 20))
CPHASE=$(K get -n "$NS" pod "$CP" -o jsonpath='{.status.phase}' 2>/dev/null)
CRST=$(K get -n "$NS" pod "$CP" -o jsonpath='{.status.containerStatuses[?(@.name=="syncer")].restartCount}' 2>/dev/null)
CSID=$(sfield "$CP" .serverId)
if [ "$CPHASE" = Running ] && [ -n "$CSID" ]; then
  ok "the challenger is alive and answering after $((TERM + 20))s (restarts=${CRST:-0}, id=$CSID)"
else
  inc "the challenger is $CPHASE and answers ${CSID:-nothing} — P1 cannot distinguish a lease from a dead pod"
fi
[ -n "$CSID" ] && [ "$CSID" != "$ID0" ] \
  && ok "and it is a DIFFERENT incarnation ($CSID != $ID0) — it took the takeover path" \
  || inc "the challenger's id is not distinguishable from the holder's"

# THE STRONGEST CONTROL AVAILABLE, and it is the mechanism itself rather
# than a proxy for it. The challenger logs `another server holds <p>
# (N/6 quiet polls)` every heartbeat. That it is being PRINTED proves the
# challenger is awake and reading the cell; that N stays at 0 proves the
# holder's token is still moving under it. "Pod is Running" cannot tell
# a polling challenger from one wedged on a socket.
CLOG=$(K logs -n "$NS" "$CP" -c syncer --tail=40 2>/dev/null)
QUIET=$(printf '%s' "$CLOG" | grep -o '[0-9]\+/6 quiet polls' | tail -1)
if [ -n "$QUIET" ]; then
  ok "the challenger is polling the cell and reports $QUIET"
  case "$QUIET" in
    0/6*) ok "and the count is ZERO — the holder's token is moving under it" ;;
    *)    bad "the quiet count reached $QUIET while the holder was renewing" ;;
  esac
else
  inc "the challenger logged no quiet-poll line; it may not be observing the cell at all"
fi
[ "$(sraw "$CP" .phase)" = claimingEpoch ] \
  && ok "and it is parked in claimingEpoch, not serving" \
  || note "challenger phase is $(sraw "$CP" .phase)"

BID1=$(bucket_holder); E1=$(bucket_epochno)
[ "$BID1" = "$BID0" ] && ok "the bucket still names the original holder" \
  || bad "the lease moved to '$BID1' while the holder was renewing — SPLIT BRAIN"
[ "$E1" = "$E0" ] && ok "and the epoch did not advance (still $E0)" \
  || bad "the epoch went $E0 -> $E1 with a live holder"
[ "$(sraw "$CP" '.epoch.held')" = true ] \
  && bad "the challenger claims to hold the lease as well — SPLIT BRAIN" \
  || ok "the challenger does not claim the lease"

PUSHOUT=$(agent_push "$M_P1" 2>&1); sleep 5
SEQ2=$(sfield "$H0" '.repo.snapshotSeq')
[ "${SEQ2:-0}" -gt "${SEQ1:-0}" ] && ok "pushes keep landing while the challenger waits" \
  || { bad "the repository stopped accepting pushes when a challenger appeared"
       note "the client said: $(printf '%s' "$PUSHOUT" | tr '\n' ' ' | cut -c1-200)"; }

# THE POSITIVE CONTROL FOR P4, and it must run BEFORE the partition.
# Exactly the path P4 uses — port-forward, clone from that pod, commit,
# push back — against a pod that still holds the lease. If this cannot
# land, P4's refusal is not evidence of fencing; it is evidence the
# drill cannot push into a pod at all.
direct_push "$H0" "$M_DIRECT"
if [ "$DPUSH_RC" -eq 0 ]; then
  ok "a direct push into the holding pod LANDS — P4's path works when the lease is held"
  P4_ARMED=yes
else
  P4_ARMED=no
  inc "the direct path cannot push into a healthy holder: $(printf '%s' "$DPUSH_OUT" | tr '\n' ' ' | cut -c1-140)"
fi

# ── P2: a partitioned holder stands down without restarting ──────────
hdr "P2: the holder loses S3 — X13's signature is ready=false with NO restart"
RST_BEFORE=$(K get -n "$NS" pod "$H0" -o jsonpath='{.status.containerStatuses[?(@.name=="syncer")].restartCount}' 2>/dev/null)
# THE CLOCK STARTS HERE, at the partition — not at the top of P3.
# The first run timed the takeover from after P2 had ALREADY waited a
# full term for `renewalOverdue`, so it measured the tail of the window
# (6s) against the whole term (60s) and called a correct lease eager.
# The challenger's own log settled it: 0/6 through 21:37:25, then one
# poll every ~10s, `holding at epoch 2` at 21:38:26 — 61s after the
# holder's token stopped moving, which is exactly QUIET_POLLS.
T_CUT=$(date +%s)
K label -n "$NS" pod "$H0" chaos=blocked --overwrite >/dev/null 2>&1
cat <<NPEOF | K apply -f - >/dev/null 2>&1
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata: { name: $NP, namespace: $NS }
spec:
  podSelector: { matchLabels: { chaos: blocked } }
  policyTypes: ["Egress"]
  egress:
    - to: [ { namespaceSelector: {} } ]
      ports: [ { protocol: UDP, port: 53 }, { protocol: TCP, port: 53 } ]
NPEOF

OVERDUE=""
for _ in $(seq 1 $(( (TERM * 3) / 5 ))); do
  OVERDUE=$(sfield "$H0" '.epoch.renewalOverdue')
  [ "$OVERDUE" = true ] && break
  sleep 5
done
if [ "$OVERDUE" = true ]; then
  ok "the holder's renewal is overdue — the partition is REAL and enforced"
else
  inc "renewalOverdue never became true; the NetworkPolicy is not being enforced (CNI?) — P3 and P4 cannot mean anything"
  cleanup; echo; echo "F16: $PASS passed, $FAIL failed, $INC inconclusive"; exit 1
fi
RST_AFTER=$(K get -n "$NS" pod "$H0" -o jsonpath='{.status.containerStatuses[?(@.name=="syncer")].restartCount}' 2>/dev/null)
[ "${RST_AFTER:-0}" = "${RST_BEFORE:-0}" ] \
  && ok "and it did NOT restart (${RST_BEFORE:-0}) — the lease is kept, the process stays up" \
  || bad "the syncer restarted ${RST_BEFORE:-0} -> ${RST_AFTER:-0}; a crash loop is the OLD vacuous pass"

# ── P3: a QUIET holder IS superseded ─────────────────────────────────
hdr "P3: the challenger supersedes, but only after the quiet window"
BID2=""; TOOK=""
for _ in $(seq 1 $(( (TERM * 6) / 5 ))); do
  BID2=$(bucket_holder)
  [ -n "$BID2" ] && [ "$BID2" != "$BID0" ] && { TOOK=$(( $(date +%s) - T_CUT )); break; }
  sleep 5
done
if [ -n "$TOOK" ]; then
  ok "the lease moved to the challenger ${TOOK}s after the partition"
  E2=$(bucket_epochno)
  [ "${E2:-0}" -gt "${E0:-0}" ] && ok "and the epoch advanced $E0 -> $E2" \
    || bad "the holder changed but the epoch did not advance ($E0 -> ${E2:-?})"
  [ "$BID2" = "$CSID" ] && ok "the new holder is the challenger" \
    || bad "the lease went to '$BID2', which is neither holder nor challenger"
  [ "$TOOK" -ge "$TERM" ] \
    && ok "the takeover waited at least one term (${TOOK}s >= ${TERM}s) — it is not eager" \
    || bad "the takeover happened in ${TOOK}s, inside the ${TERM}s term — a live straggler could still hold an If-Match"
else
  bad "no takeover within $((TERM * 6))s — a dead holder's repository is stuck"
fi

# ── P4: the deposed holder cannot write when S3 comes back ───────────
hdr "P4: the ZOMBIE — S3 returns to a holder that has been deposed"
K delete -n "$NS" networkpolicy "$NP" >/dev/null 2>&1
K label -n "$NS" pod "$H0" chaos- >/dev/null 2>&1
ok "the partition is healed; the deposed pod has S3 again"

MARK="f16-zombie-$RUN"
if [ "${P4_ARMED:-no}" != yes ]; then
  inc "the direct path never landed against a healthy holder, so a refusal here would not be evidence — P4 is not armed"
else
  direct_push "$H0" "$MARK"
  note "the deposed pod answered: $(printf '%s' "$DPUSH_OUT" | tr '\n' ' ' | cut -c1-160)"
  # The oracle is the exit status, and it is ONE value — but only after
  # ruling out the case where nothing reached the pod at all. A
  # transport failure and a fencing both exit non-zero, and only one of
  # them is evidence. The control above already proved this path can
  # reach a pod, so a transport failure now is a drill fault.
  # WHAT PROVES THE FRONT ANSWERED is `[remote rejected]` (or `To <url>`),
  # not the absence of "Connection refused" — forge's own refusal CARRIES
  # that phrase as its reason: the deposed syncer stops serving its hook
  # socket, so `proc-receive` reaches nothing and reports
  # `the repository server is not accepting writes (Connection refused)`.
  # The first run matched on the reason and called a correct refusal
  # inconclusive.
  if [ "$(count "$DPUSH_OUT" '\[remote rejected\]')" = 0 ] \
     && [ "$(count "$DPUSH_OUT" "^To ")" = 0 ]; then
    inc "the push never reached the deposed pod's git front — 'it did not write' proves nothing here"
  elif [ "$DPUSH_RC" -ne 0 ]; then
    ok "the deposed holder REFUSED a push the SAME path landed while it held the lease"
  else
    bad "the deposed holder ACCEPTED a push after being superseded — TWO WRITERS"
  fi
fi
sleep 15
BID3=$(bucket_holder)
[ "$BID3" = "$BID2" ] && ok "the bucket still names the successor" \
  || bad "the lease went back to '$BID3' — the zombie re-acquired"
# WHAT SAFETY REQUIRES is that the deposed pod no longer claims to be the
# writer — NOT that `fenced` is set. Observed on the wire: a deposed
# holder drops back to `claimingEpoch` at 0/6 and warms from the batch
# log as a standby, leaving `fenced` null. That is the better behaviour
# and the first run's expectation was simply wrong about the mechanism.
DHELD=$(sraw "$H0" '.epoch.held'); DPHASE=$(sraw "$H0" .phase); FENCED=$(sraw "$H0" .fenced)
[ "$DHELD" != true ] \
  && ok "the deposed pod no longer claims the lease (phase=$DPHASE fenced=$FENCED)" \
  || bad "the deposed pod STILL claims to hold the lease — TWO WRITERS"
[ "$(sraw "$H0" .serverId)" != "$BID2" ] \
  && ok "and the bucket's holder is a different server than the deposed one" \
  || bad "the deposed pod's id is the bucket's holder"

NP2=$(holder_pod) || NP2=""
if [ -n "$NP2" ]; then
  n=$(K -n "$NS" exec "$NP2" -c syncer -- sh -c "git --git-dir=/repo/$NS/$REPO.git log --all --oneline 2>/dev/null | grep -c $MARK" 2>/dev/null)
  [ "${n:-0}" = 0 ] \
    && ok "the zombie's commit is in NO ref the successor serves" \
    || bad "the zombie's commit reached the repository (${n} refs) — a deposed writer wrote"
else
  inc "no pod matches the bucket's holder; cannot check for the zombie's commit"
fi

# ── P5: nothing acknowledged was lost ────────────────────────────────
hdr "P5: every acknowledged push survived the takeover"
if [ -n "$NP2" ]; then
  for m in "$M_P0" "$M_P1" "$M_DIRECT"; do
    n=$(K -n "$NS" exec "$NP2" -c syncer -- sh -c "git --git-dir=/repo/$NS/$REPO.git log --all --oneline 2>/dev/null | grep -c $m" 2>/dev/null)
    [ "${n:-0}" -ge 1 ] && ok "$m survived" || bad "$m was acknowledged before the partition and is GONE"
  done
else
  inc "no serving pod to read the history from"
fi

# ── P6: and the bucket alone still rebuilds it ───────────────────────
hdr "P6: a fresh syncer rebuilds from S3 alone"
# Deleting the lease HOLDER gracefully also measures the other half of
# the protocol: `epoch_release` on preStop is a clean handoff, and a
# successor may take it AT ONCE rather than waiting out a quiet window.
# P3's ${TOOK}s is the control — if this is not markedly faster, the
# release is doing nothing and every roll costs a term of downtime.
T_CLEAN=$(date +%s)
K delete -n "$NS" deploy "$CHAL" --wait=false >/dev/null 2>&1
OLDUID=$(K get -n "$NS" pod "$H0" -o jsonpath='{.metadata.uid}' 2>/dev/null)
K delete -n "$NS" pod "$H0" --wait=false >/dev/null 2>&1
NEWP=""; NEWUID=""
for _ in $(seq 1 48); do
  NEWP=$(repo_pods | head -1)
  [ -n "$NEWP" ] && NEWUID=$(K get -n "$NS" pod "$NEWP" -o jsonpath='{.metadata.uid}' 2>/dev/null)
  [ -n "$NEWUID" ] && [ "$NEWUID" != "$OLDUID" ] && \
    [ "$(K get -n "$NS" pod "$NEWP" -o jsonpath='{.status.containerStatuses[?(@.name=="syncer")].ready}' 2>/dev/null)" = true ] && break
  sleep 5
done
CLEAN=$(( $(date +%s) - T_CLEAN ))
if [ -n "$NEWUID" ] && [ "$NEWUID" != "$OLDUID" ]; then
  ok "a FRESH pod came up (uid changed) — not the old process still answering"
  note "clean handover + restore took ${CLEAN}s; P3's quiet-window takeover took ${TOOK:-?}s (term ${TERM}s)"
  is_up "$NEWP" \
    && ok "and it is serving from the bucket alone (phase=$(sraw "$NEWP" .phase))" \
    || bad "the fresh syncer never took the lease: phase=$(sraw "$NEWP" .phase) held=$(sraw "$NEWP" '.epoch.held')"
  for m in "$M_P0" "$M_P1" "$M_DIRECT"; do
    n=$(K -n "$NS" exec "$NEWP" -c syncer -- sh -c "git --git-dir=/repo/$NS/$REPO.git log --all --oneline 2>/dev/null | grep -c $m" 2>/dev/null)
    [ "${n:-0}" -ge 1 ] && ok "$m is in the rebuilt repository" || bad "$m did not survive the rebuild"
  done
  n=$(K -n "$NS" exec "$NEWP" -c syncer -- sh -c "git --git-dir=/repo/$NS/$REPO.git log --all --oneline 2>/dev/null | grep -c $MARK" 2>/dev/null)
  [ "${n:-0}" = 0 ] && ok "and the zombie's commit is not in it either" \
    || bad "the zombie's commit came back from S3 — it DID reach the bucket"
else
  inc "no fresh pod appeared; restorability was not tested"
fi

hdr "the biconditional"
if [ -n "$TOOK" ]; then
  ok "both arms ran: no takeover while renewing, takeover after ${TOOK}s of quiet"
else
  inc "only the no-takeover arm produced a result — P1 alone cannot distinguish a lease from a dead challenger"
fi

K delete -n "$NS" flintrepo "$REPO" --wait=false >/dev/null 2>&1
echo
echo "==== F16: $PASS passed, $FAIL failed, $INC inconclusive ===="
[ "$FAIL" -eq 0 ] && [ "$INC" -eq 0 ] || exit 1
