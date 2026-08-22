#!/usr/bin/env bash
# git operations drill — the git a real agent actually runs.
#
# WHY A WHOLE DRILL FOR ONE TOOL
#
# The workload drill proves `git init`, three commits and a branch work.
# That is the easy path, and it is not what breaks on a network
# filesystem. Git's hard parts are the ones that lean on POSIX
# guarantees NFS implements differently:
#
#   * `index.lock` is created with O_EXCL. That is git's ONLY mutual
#     exclusion, and it is exactly what two agents sharing a workspace
#     will collide on. The correct outcome is one winner and one CLEAN
#     refusal — never two winners, and never a corrupt index.
#   * `gc`/`repack` rewrite the object store: hundreds of creates, a
#     rename of the pack into place, then deletes of the loose objects.
#   * `checkout` between divergent branches is bulk create + unlink.
#   * `clone` is the create-heavy path an agent runs on every task.
#
# EVERY LEG ENDS IN `git fsck`. "The command exited 0" is not the claim;
# "the repository is still structurally sound" is. A filesystem that
# loses a rename can leave git exit 0 and the repo quietly broken.
#
#   MODE=cluster BUCKET=... REGION=... DRILL_AK=... DRILL_SK=... \
#     ./tests/regression/git-ops-drill.sh
set -uo pipefail

NS="${NS:-workspaces}"
OPNS="${OPNS:-flint-system}"
PROJECT="${PROJECT:-gitops}"
SHARE="fs-$PROJECT"
PVC_SIZE="${PVC_SIZE:-8Gi}"
NFILES="${NFILES:-400}"
BUCKET="${BUCKET:?needs BUCKET}"
REGION="${REGION:-us-west-1}"
HUB_AK="${DRILL_AK:?needs DRILL_AK}"
HUB_SK="${DRILL_SK:?needs DRILL_SK}"
export HELM_CACHE_HOME="${HELM_CACHE_HOME:-${TMPDIR:-/tmp}/flint-drill-helm-cache}"
mkdir -p "$HELM_CACHE_HOME" 2>/dev/null

PASSES=0; FAILURES=()
say()  { echo; echo "── $*"; }
pass() { echo "  ✓ $*"; PASSES=$((PASSES+1)); }
fail() { echo "  ✗ FAIL: $*"; exit 1; }
bad()  { echo "  ✗ FAIL: $*"; FAILURES+=("$*"); }
note() { echo "    · $*" >&2; }
s3()   { AWS_DEFAULT_REGION="$REGION" aws "$@" 2>&1; }

cleanup() {
  set +e
  [ "${KEEP:-0}" = "1" ] && { echo "KEEP=1 — objects left standing"; return; }
  kubectl -n "$NS" delete pod gita gitb --force --grace-period=0 >/dev/null 2>&1
  kubectl -n "$NS" delete flintshare "$SHARE" --ignore-not-found >/dev/null 2>&1
  kubectl -n "$NS" delete pvc gitops-pvc "$SHARE-data" --ignore-not-found --timeout=120s >/dev/null 2>&1
  kubectl delete pv gitops-pv --ignore-not-found >/dev/null 2>&1
  kubectl -n "$NS" delete secret "tok-$PROJECT" --ignore-not-found >/dev/null 2>&1
}
trap cleanup EXIT

gw_pod() {
  kubectl -n "$OPNS" get pod -l app.kubernetes.io/name=flint-lite-operator-gateway \
    --field-selector=status.phase=Running \
    -o jsonpath='{range .items[?(@.status.containerStatuses[0].ready==true)]}{.metadata.name}{"\n"}{end}' 2>/dev/null | head -1
}
derive_for() {
  local pod out
  for _ in 1 2 3 4 5; do
    pod=$(gw_pod)
    if [ -n "$pod" ]; then
      out=$(kubectl -n "$OPNS" exec "$pod" -- /usr/local/bin/flint-hub-gateway \
        --root-key-file=/etc/flint/gateway-root/key --derive-for "$1" 2>/tmp/g-err.txt | tr -d '\r\n')
      [ -n "$out" ] && { printf '%s' "$out"; return 0; }
    fi; sleep 4
  done; return 1
}

echo "══════════════════════════════════════════════════════════════════"
echo " git operations drill — clone, checkout, merge, gc, concurrent lock"
echo "══════════════════════════════════════════════════════════════════"

say "preflight"
helm status flint-lite-operator -n "$OPNS" >/dev/null 2>&1 || fail "operator not installed"
n=$(s3 s3 ls "s3://$BUCKET/$PROJECT/" --recursive 2>/dev/null | grep -c . || true)
[ "${n:-0}" -eq 0 ] || fail "s3://$BUCKET/$PROJECT/ already has $n object(s)"
kubectl -n "$NS" create secret generic flint-s3 \
  --from-literal=AWS_ACCESS_KEY_ID="$HUB_AK" --from-literal=AWS_SECRET_ACCESS_KEY="$HUB_SK" \
  --dry-run=client -o yaml | kubectl apply -f - >/dev/null
kubectl -n "$NS" apply -f - >/dev/null <<EOF || fail "FlintShare refused"
apiVersion: flint.io/v1alpha1
kind: FlintShare
metadata: { name: $SHARE, namespace: $NS, labels: { flint.io/project-id: $PROJECT } }
spec:
  bucket: $BUCKET
  keyPrefix: $PROJECT/
  region: $REGION
  credentialsSecretRef: flint-s3
  persistence: { size: $PVC_SIZE }
  monitoring:
    enabled: true
    fileApi: { enabled: true, tokenSecretRef: tok-$PROJECT }
  settings: { flushFloorSecs: 30 }
EOF
T=$(derive_for "$NS/$SHARE") || { tail -3 /tmp/g-err.txt; fail "derive-for failed"; }
kubectl -n "$NS" create secret generic "tok-$PROJECT" --from-literal=token="$T" \
  --dry-run=client -o yaml | kubectl apply -f - >/dev/null
for _ in $(seq 1 60); do
  [ "$(kubectl -n "$NS" get flintshare "$SHARE" -o jsonpath='{.status.phase}' 2>/dev/null)" = Ready ] && break; sleep 5
done
[ "$(kubectl -n "$NS" get flintshare "$SHARE" -o jsonpath='{.status.phase}')" = Ready ] || fail "share never Ready"
CIP=$(kubectl -n "$NS" get svc "$SHARE" -o jsonpath='{.spec.clusterIP}')
pass "share Ready (ClusterIP $CIP)"

kubectl apply -f - >/dev/null <<EOF || fail "PV/PVC refused"
apiVersion: v1
kind: PersistentVolume
metadata: { name: gitops-pv }
spec:
  capacity: { storage: 100Gi }
  accessModes: [ReadWriteMany]
  persistentVolumeReclaimPolicy: Retain
  mountOptions: [nfsvers=4.1, proto=tcp, hard, nconnect=4, noatime, sec=sys]
  nfs: { server: $CIP, path: / }
---
apiVersion: v1
kind: PersistentVolumeClaim
metadata: { name: gitops-pvc, namespace: $NS }
spec:
  accessModes: [ReadWriteMany]
  storageClassName: ""
  volumeName: gitops-pv
  resources: { requests: { storage: 100Gi } }
EOF
NODES=$(kubectl get nodes --no-headers -o custom-columns=N:.metadata.name | tr '\n' ' ')
N1=$(echo "$NODES" | awk '{print $1}'); N2=$(echo "$NODES" | awk '{print ($2==""?$1:$2)}')
mkpod() {
  kubectl -n "$NS" apply -f - >/dev/null <<EOF || fail "pod $1 refused"
apiVersion: v1
kind: Pod
metadata: { name: $1, namespace: $NS }
spec:
  nodeName: $2
  restartPolicy: Never
  containers:
    - name: c
      image: debian:12-slim
      command: ["sh","-c","sleep 7200"]
      volumeMounts: [{ name: ws, mountPath: /workspace }]
  volumes:
    - name: ws
      persistentVolumeClaim: { claimName: gitops-pvc }
EOF
}
mkpod gita "$N1"; mkpod gitb "$N2"
kubectl -n "$NS" wait --for=condition=ready pod/gita --timeout=240s >/dev/null 2>&1 || fail "gita never Ready"
kubectl -n "$NS" wait --for=condition=ready pod/gitb --timeout=240s >/dev/null 2>&1 || fail "gitb never Ready"
GA() { kubectl -n "$NS" exec gita -- sh -c "$1" 2>&1; }
GB() { kubectl -n "$NS" exec gitb -- sh -c "$1" 2>&1; }
GA 'apt-get update -qq >/dev/null 2>&1; apt-get install -y -qq git >/dev/null 2>&1' >/dev/null
GB 'apt-get update -qq >/dev/null 2>&1; apt-get install -y -qq git >/dev/null 2>&1' >/dev/null
V=$(GA 'command -v git >/dev/null && git --version' | tr -d '\r')
case "$V" in *"git version"*) ;; *) fail "git not installed in gita ($V) — RIG, not the product" ;; esac
VB=$(GB 'command -v git >/dev/null && git --version' | tr -d '\r')
case "$VB" in *"git version"*) ;; *) fail "git not installed in gitb ($VB) — RIG, not the product" ;; esac
note "gita on $N1 ($V); gitb on $N2 ($VB)"
GENV='export HOME=/tmp GIT_AUTHOR_NAME=d GIT_AUTHOR_EMAIL=d@e GIT_COMMITTER_NAME=d GIT_COMMITTER_EMAIL=d@e; git config --global init.defaultBranch main >/dev/null 2>&1; git config --global user.email d@e >/dev/null 2>&1; git config --global user.name d >/dev/null 2>&1;'
pass "two git clients on ${N1}${N2:+ and $N2}"

fsck_ok() {  # fsck_ok <repo> -> "OK" or the error
  GA "$GENV cd $1 && git fsck --no-progress --strict 2>&1 | grep -viE '^(checking|dangling)' | head -3; echo FSCKDONE" \
    | sed 's/FSCKDONE//' | tr -d '\r' | tr -s ' \n' ' ' | sed 's/^ *//;s/ *$//'
}

# ══ G1: a repo with real bulk ════════════════════════════════════════
say "G1: build a repo with $NFILES files across 3 commits"
OUT=$(GA "$GENV set -e
  rm -rf /workspace/big && mkdir -p /workspace/big && cd /workspace/big
  git init -q .
  mkdir -p src/a src/b docs
  echo seed > docs/note.txt      # git does NOT track empty directories:
                                 # without a file here, docs/ vanishes on
                                 # checkout and G3 fails on the mount for
                                 # a reason that has nothing to do with NFS
  for i in \$(seq 1 $NFILES); do echo \"line \$i\" > src/a/f\$i.txt; done
  git add -A && git commit -qm bulk1
  for i in \$(seq 1 $((NFILES/2))); do echo \"more \$i\" >> src/a/f\$i.txt; done
  git add -A && git commit -qm bulk2
  for i in \$(seq 1 $((NFILES/4))); do echo x > src/b/g\$i.txt; done
  git add -A && git commit -qm bulk3
  git rev-list --count HEAD; git ls-files | wc -l; echo G1_OK")
NCOM=$(echo "$OUT" | grep -E '^[0-9]+$' | head -1); NF=$(echo "$OUT" | grep -E '^[0-9]+$' | sed -n 2p)
FS=$(fsck_ok /workspace/big)
case "$OUT" in
  *G1_OK*) if [ "$NCOM" = "3" ] && [ "${NF:-0}" -ge "$NFILES" ] && [ -z "$FS" ]; then
             pass "3 commits, $NF tracked files, fsck --strict clean"
           else bad "repo built but inconsistent (commits=$NCOM files=$NF fsck='$FS')"; fi ;;
  *) note "$(echo "$OUT"|tail -4)"; bad "building a $NFILES-file repo failed" ;;
esac

# ══ G2: clone, on the mount and off it ═══════════════════════════════
say "G2: clone — mount→mount, and mount→local disk"
OUT=$(GA "$GENV set -e
  rm -rf /workspace/clone1 /tmp/clone2
  git clone -q /workspace/big /workspace/clone1
  git clone -q /workspace/big /tmp/clone2
  cd /workspace/clone1 && git rev-list --count HEAD
  cd /tmp/clone2 && git rev-list --count HEAD
  echo G2_OK")
C1=$(echo "$OUT" | grep -E '^[0-9]+$' | head -1); C2=$(echo "$OUT" | grep -E '^[0-9]+$' | sed -n 2p)
FS=$(fsck_ok /workspace/clone1)
case "$OUT" in
  *G2_OK*) if [ "$C1" = "3" ] && [ "$C2" = "3" ] && [ -z "$FS" ]; then
             pass "clone onto the mount and off it both reproduce 3 commits; clone fsck clean"
           else bad "clone counts wrong (on-mount=$C1 off-mount=$C2 fsck='$FS')"; fi ;;
  *) note "$(echo "$OUT"|tail -4)"; bad "git clone failed" ;;
esac

# ══ G3: branch divergence + checkout + merge with a conflict ═════════
say "G3: divergent branches, checkout churn, and a real merge conflict"
OUT=$(GA "$GENV set -e
  cd /workspace/big
  git checkout -q -b feat
  for i in \$(seq 1 50); do echo feat > src/a/f\$i.txt; done
  mkdir -p docs && echo CONFLICT-FEAT > docs/note.txt
  git add -A && git commit -qm feat1
  git checkout -q main
  mkdir -p docs && echo CONFLICT-MAIN > docs/note.txt
  git add -A && git commit -qm main1
  git checkout -q feat && git checkout -q main   # bulk churn both ways
  if git merge -q feat 2>/dev/null; then echo MERGED_CLEAN; else echo CONFLICTED; fi
  git status --porcelain | grep -c '^UU' || true
  echo CONFLICT-RESOLVED > docs/note.txt
  git add docs/note.txt && git commit -qm resolve
  git rev-list --count HEAD
  echo G3_OK")
CONF=$(echo "$OUT" | grep -c 'CONFLICTED')
FS=$(fsck_ok /workspace/big)
case "$OUT" in
  *G3_OK*) if [ "$CONF" -ge 1 ] && [ -z "$FS" ]; then
             pass "checkout churn + a real merge conflict raised, resolved and committed; fsck clean"
           else bad "merge did not behave as expected (conflicted=$CONF fsck='$FS')"; fi ;;
  *) note "$(echo "$OUT"|tail -5)"; bad "branch/merge sequence failed on the mount" ;;
esac

# ══ G4: gc / repack — the rename-and-delete storm ════════════════════
say "G4: git gc --aggressive — hundreds of creates, a pack rename, loose-object deletes"
LOOSE_BEFORE=$(GA "cd /workspace/big && find .git/objects -type f -name '*' | grep -vc pack" | tr -d ' \r')
OUT=$(GA "$GENV set -e
  cd /workspace/big
  git gc --aggressive --prune=now -q 2>&1 | tail -2
  ls .git/objects/pack/*.pack 2>/dev/null | wc -l
  git rev-list --count HEAD
  echo G4_OK")
PACKS=$(echo "$OUT" | grep -E '^[0-9]+$' | head -1)
FS=$(fsck_ok /workspace/big)
case "$OUT" in
  *G4_OK*) if [ "${PACKS:-0}" -ge 1 ] && [ -z "$FS" ]; then
             pass "gc --aggressive packed the repo (${PACKS} pack, ${LOOSE_BEFORE} loose objects before) and fsck is clean"
           else bad "gc left the repo in a bad state (packs=$PACKS fsck='$FS')"; fi ;;
  *) note "$(echo "$OUT"|tail -4)"; bad "git gc failed on the mount" ;;
esac

# ══ G5: TWO AGENTS, ONE REPO — index.lock ════════════════════════════
say "G5: two agents commit to ONE repo at once — index.lock is git's only mutex"
# THE agent-fleet hazard. git guards its index with an O_EXCL create of
# .git/index.lock. If the filesystem lets both callers create it, both
# proceed and the index is corrupted. The CORRECT outcome is one winner
# and one clean refusal — not two winners.
GA "$GENV cd /workspace/big && git checkout -q main" >/dev/null
GB "$GENV cd /workspace/big && git status >/dev/null 2>&1" >/dev/null
RA=/tmp/g5a.txt; RB=/tmp/g5b.txt
# Scoped wait, for the same reason as the workload drill: a bare `wait`
# would also wait on any long-lived background child.
( GA "$GENV cd /workspace/big && echo a-\$(date +%s%N) > racea.txt && git add racea.txt && git commit -qm racea && echo A_WON || echo A_REFUSED" >"$RA" 2>&1 ) & RPID_A=$!
( GB "$GENV cd /workspace/big && echo b-\$(date +%s%N) > raceb.txt && git add raceb.txt && git commit -qm raceb && echo B_WON || echo B_REFUSED" >"$RB" 2>&1 ) & RPID_B=$!
wait "$RPID_A" "$RPID_B"
# `grep -c` already prints a count AND exits 1 on no match, so a
# `|| echo 0` fallback appends a SECOND line and the arithmetic below
# chokes on "0\n0". `|| true` swallows the status without adding output.
AW=$(grep -c A_WON "$RA" 2>/dev/null || true); AW=$(printf '%s' "${AW:-0}" | head -1)
BW=$(grep -c B_WON "$RB" 2>/dev/null || true); BW=$(printf '%s' "${BW:-0}" | head -1)
WINS=$(( ${AW:-0} + ${BW:-0} ))
note "agent-a: $(tr -d '\r' <"$RA" | tail -1)   agent-b: $(tr -d '\r' <"$RB" | tail -1)"
FS=$(fsck_ok /workspace/big)
if [ -n "$FS" ]; then
  bad "after a concurrent commit race the repo is NOT fsck-clean: $FS"
elif [ "$WINS" -eq 0 ]; then
  bad "neither agent committed — both were refused, which is a liveness failure not a safety one"
else
  pass "$WINS of 2 concurrent committers succeeded and the repo is still fsck-clean (git's O_EXCL lock held)"
  [ "$WINS" -eq 1 ] && note "exactly one winner — textbook index.lock behaviour" \
                     || note "both succeeded: they serialised rather than collided, which is also correct"
fi

# ══ G6: worktrees and a fetch between two repos on the mount ═════════
say "G6: git worktree, and fetch between two repos on the mount"
OUT=$(GA "$GENV set -e
  cd /workspace/big
  git worktree add -q /workspace/wt main 2>/dev/null || git worktree add -q /workspace/wt -b wtbranch
  cd /workspace/wt && git rev-list --count HEAD
  cd /workspace/clone1 && git remote set-url origin /workspace/big 2>/dev/null || git remote add origin /workspace/big
  git fetch -q origin 2>&1 | tail -1
  git rev-list --count origin/main 2>/dev/null || git rev-list --count FETCH_HEAD
  echo G6_OK")
WT=$(echo "$OUT" | grep -E '^[0-9]+$' | head -1); FE=$(echo "$OUT" | grep -E '^[0-9]+$' | sed -n 2p)
case "$OUT" in
  *G6_OK*) if [ "${WT:-0}" -ge 3 ] && [ "${FE:-0}" -ge 3 ]; then
             pass "worktree checked out ($WT commits) and a mount→mount fetch brought $FE commits"
           else bad "worktree/fetch counts wrong (worktree=$WT fetch=$FE)"; fi ;;
  *) note "$(echo "$OUT"|tail -4)"; bad "worktree or fetch failed on the mount" ;;
esac

# ══ G7: how long does git actually take here? ════════════════════════
say "G7: timings, for expectation-setting rather than pass/fail"
T1=$(GA "$GENV cd /workspace/big && S=\$(date +%s%N); git status --porcelain >/dev/null; E=\$(date +%s%N); echo \$(( (E-S)/1000000 ))" | tr -d ' \r')
T2=$(GA "$GENV cd /workspace/big && S=\$(date +%s%N); git checkout -q feat; git checkout -q main; E=\$(date +%s%N); echo \$(( (E-S)/1000000 ))" | tr -d ' \r')
T3=$(GA "$GENV rm -rf /tmp/c3; S=\$(date +%s%N); git clone -q /workspace/big /tmp/c3; E=\$(date +%s%N); echo \$(( (E-S)/1000000 ))" | tr -d ' \r')
note "git status (${NFILES}+ files): ${T1}ms"
note "checkout feat→main→feat:      ${T2}ms"
note "clone mount→local:            ${T3}ms"
pass "timings captured"

echo
echo "══════════════════════════════════════════════════════════════════"
echo " git operations summary — $PASSES checks passed"
echo "══════════════════════════════════════════════════════════════════"
echo " repo size                 : ${NFILES} files, 3+ commits"
echo " git status                : ${T1:-?}ms"
echo " checkout churn (2 way)    : ${T2:-?}ms"
echo " clone mount→local         : ${T3:-?}ms"
echo " concurrent committers won : ${WINS:-?} of 2"
echo
if [ ${#FAILURES[@]} -eq 0 ]; then echo "ALL LEGS PASSED."; else
  echo "${#FAILURES[@]} LEG(S) FAILED:"; for f in "${FAILURES[@]}"; do echo "  ✗ $f"; done; exit 1; fi
